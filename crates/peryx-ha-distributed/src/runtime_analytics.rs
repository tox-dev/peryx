use std::sync::Arc;
use std::time::Duration;

use crate::{
    AnalyticsReceiver, AuthorityEpoch, DEFAULT_APPLY_LIMITS, HttpAnalyticsSource, ProducerId, TransferLimits, pull,
};
use anyhow::Context as _;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use peryx_ha::AnalyticsBatchSource;
use peryx_storage::meta::AnalyticsHandle;
use serde::{Deserialize, Serialize};

/// Callers retry fetch timeouts on the next poll.
const ANALYTICS_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// A durable epoch keeps re-served days replay-identical across producer restarts.
#[derive(Debug, Serialize, Deserialize)]
struct ProducerRecord {
    epoch: u64,
}

/// Reuses the stored epoch; initializes epoch one when the record is absent or malformed.
///
/// # Errors
/// Returns store errors from reading or writing the record.
pub fn resolve_producer_epoch(analytics: &AnalyticsHandle) -> anyhow::Result<AuthorityEpoch> {
    if let Some(bytes) = analytics.load_producer().context("read analytics producer record")?
        && let Ok(record) = serde_json::from_slice::<ProducerRecord>(&bytes)
    {
        return Ok(AuthorityEpoch(record.epoch));
    }
    let record = ProducerRecord { epoch: 1 };
    let bytes = serde_json::to_vec(&record).context("encode analytics producer record")?;
    analytics
        .save_producer(&bytes)
        .context("persist analytics producer record")?;
    Ok(AuthorityEpoch(record.epoch))
}

#[derive(Clone)]
struct EndpointState {
    token: Arc<str>,
    source: Arc<dyn AnalyticsBatchSource>,
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
    let batches = state.source.sealed_batches(&state.producer, state.epoch, query.after);
    Json(batches).into_response()
}

/// Serves sealed batches after a requested day and requires the replication bearer token.
pub fn analytics_router(
    token: impl Into<String>,
    source: Arc<dyn AnalyticsBatchSource>,
    producer: ProducerId,
    epoch: AuthorityEpoch,
) -> Router {
    let state = EndpointState {
        token: Arc::from(token.into()),
        source,
        producer,
        epoch,
    };
    Router::new()
        .route("/+replication/v1/analytics", get(serve_analytics))
        .with_state(state)
}

type PersistApply = Box<dyn Fn(&[u8]) -> anyhow::Result<()> + Send + Sync>;

/// Persists receiver state after any pull that applies a batch.
pub struct AnalyticsPuller {
    source: HttpAnalyticsSource,
    persist: PersistApply,
    receiver: AnalyticsReceiver,
    poll_interval: Duration,
}

impl AnalyticsPuller {
    /// # Errors
    /// Returns an error when the upstream URL is unusable or receiver restore fails.
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

    async fn pull_once(&mut self) {
        let mut staged = self.receiver.clone();
        match pull(&self.source, &mut staged).await {
            Ok(report) if report.applied > 0 => match (self.persist)(&staged.encode()) {
                Ok(()) => {
                    self.receiver = staged;
                    tracing::info!(applied = report.applied, "analytics batches applied");
                }
                Err(error) => tracing::error!(%error, "persist analytics apply state failed"),
            },
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "analytics pull failed"),
        }
    }

    /// Runs until cancellation, waiting `poll_interval` after each pull. Logs pull and persistence
    /// failures; the worker continues on the next poll.
    pub async fn run(mut self) {
        loop {
            self.pull_once().await;
            tokio::time::sleep(self.poll_interval).await;
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/runtime_analytics_tests.rs"]
mod tests;
