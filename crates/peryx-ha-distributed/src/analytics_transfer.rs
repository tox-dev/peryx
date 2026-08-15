use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode, Url};

use crate::analytics::{AnalyticsBatch, AnalyticsReceiver, ApplyError, ApplyOutcome};
use crate::peer::{TransferLimits, TransportError};

#[async_trait]
pub trait AnalyticsSource: Sync {
    /// Fetch every sealed batch whose day is greater than `after_day`, in ascending day order.
    ///
    /// # Errors
    /// Returns [`TransportError`] on connection or response-validation failure.
    async fn fetch_after(&self, after_day: i64) -> Result<Vec<AnalyticsBatch>, TransportError>;
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PullReport {
    pub applied: usize,
    pub duplicate: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum AnalyticsPullError {
    #[error("could not fetch analytics batches: {0}")]
    Transport(#[source] TransportError),
    #[error("could not apply an analytics batch: {0}")]
    Apply(#[source] ApplyError),
}

/// Applies batches in source order and leaves a rejected batch unapplied.
///
/// # Errors
/// [`AnalyticsPullError::Transport`] when source fetch fails, or [`AnalyticsPullError::Apply`] when the
/// receiver refuses a batch.
pub async fn pull(
    source: &(dyn AnalyticsSource + Send),
    receiver: &mut AnalyticsReceiver,
) -> Result<PullReport, AnalyticsPullError> {
    let after = receiver.resume_day();
    let batches = source.fetch_after(after).await.map_err(AnalyticsPullError::Transport)?;
    let mut report = PullReport::default();
    for batch in &batches {
        match receiver.apply(batch).map_err(AnalyticsPullError::Apply)? {
            ApplyOutcome::Applied => report.applied += 1,
            ApplyOutcome::Duplicate => report.duplicate += 1,
        }
    }
    Ok(report)
}

const ANALYTICS_PATH: &str = "+replication/v1/analytics";
const USER_AGENT: &str = concat!("peryx-ha-distributed/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, thiserror::Error)]
pub enum HttpAnalyticsError {
    #[error("producer replication token must not be empty")]
    EmptyToken,
    #[error("invalid producer URL {0:?}")]
    InvalidBase(String),
}

/// Pulls sealed analytics batches over bearer-authenticated HTTP.
#[derive(Clone)]
pub struct HttpAnalyticsSource {
    http: Client,
    endpoint: Url,
    token: String,
    limits: TransferLimits,
}

impl fmt::Debug for HttpAnalyticsSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpAnalyticsSource")
            .field("endpoint", &self.endpoint)
            .field("limits", &self.limits)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl HttpAnalyticsSource {
    /// # Errors
    /// Returns [`HttpAnalyticsError`] for an empty token or a URL that is not a usable HTTP(S) base.
    ///
    /// # Panics
    /// Panics if reqwest rejects the static user agent, duration timeout, or active rustls provider.
    pub fn new(
        base: &str,
        token: impl Into<String>,
        limits: TransferLimits,
        timeout: Duration,
    ) -> Result<Self, HttpAnalyticsError> {
        let token = token.into();
        if token.is_empty() {
            return Err(HttpAnalyticsError::EmptyToken);
        }
        let Ok(mut base_url) = Url::parse(base) else {
            return Err(HttpAnalyticsError::InvalidBase(base.to_owned()));
        };
        if !matches!(base_url.scheme(), "http" | "https") || base_url.cannot_be_a_base() {
            return Err(HttpAnalyticsError::InvalidBase(base.to_owned()));
        }
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }
        base_url.set_query(None);
        base_url.set_fragment(None);
        let endpoint = base_url
            .join(ANALYTICS_PATH)
            .expect("a rooted analytics path always joins");
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(timeout)
            .build()
            .expect("a reqwest client with a static user agent and a duration timeout always builds");
        Ok(Self {
            http,
            endpoint,
            token,
            limits,
        })
    }
}

fn classify_loss(error: &reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout
    } else {
        TransportError::Disconnected
    }
}

#[async_trait]
impl AnalyticsSource for HttpAnalyticsSource {
    async fn fetch_after(&self, after_day: i64) -> Result<Vec<AnalyticsBatch>, TransportError> {
        let mut url = self.endpoint.clone();
        url.query_pairs_mut().append_pair("after", &after_day.to_string());
        let mut response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|error| classify_loss(&error))?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(TransportError::Unauthenticated);
        }
        if status.is_server_error() {
            return Err(TransportError::ServerError {
                status: status.as_u16(),
            });
        }
        if !status.is_success() {
            return Err(TransportError::BadStatus {
                status: status.as_u16(),
            });
        }
        let cap = self.limits.max_encoded_bytes.get();
        if let Some(length) = response.content_length()
            && length > cap
        {
            return Err(TransportError::FrameTooLarge {
                limit: cap,
                actual: length,
            });
        }
        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| classify_loss(&error))? {
            let total = body.len() as u64 + chunk.len() as u64;
            if total > cap {
                return Err(TransportError::FrameTooLarge {
                    limit: cap,
                    actual: total,
                });
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| TransportError::Malformed)
    }
}

#[cfg(test)]
#[path = "../tests/unit/analytics_transfer/tests.rs"]
mod tests;
