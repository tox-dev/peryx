//! Retry policy: which failures are worth retrying and the jittered backoff between attempts.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use url::Url;

pub(super) const MAX_RETRIES: u32 = 2;
const RETRY_BASE_MILLIS: u64 = 100;
const RETRY_CAP_MILLIS: u64 = 2_000;

pub(super) fn should_retry_status(status: StatusCode) -> bool {
    status.is_server_error() || matches!(status, StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS)
}

pub(super) fn should_retry_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect() || err.is_body() || err.is_decode()
}

pub(super) async fn sleep_before_retry(url: &Url, attempt: u32, err: &reqwest::Error) {
    sleep_before_retry_str(url.as_str(), attempt, err).await;
}

pub(super) async fn sleep_before_retry_str(url: &str, attempt: u32, err: &reqwest::Error) {
    let delay = retry_delay(attempt);
    tracing::debug!(url, error = ?err, delay_ms = delay.as_millis(), "upstream request failed, retrying");
    tokio::time::sleep(delay).await;
}

pub(super) async fn sleep_before_retry_status(url: &Url, attempt: u32, status: StatusCode) {
    let delay = retry_delay(attempt);
    tracing::debug!(%url, %status, delay_ms = delay.as_millis(), "upstream returned retryable status");
    tokio::time::sleep(delay).await;
}

fn retry_delay(attempt: u32) -> Duration {
    let cap = RETRY_CAP_MILLIS.min(RETRY_BASE_MILLIS.saturating_mul(1_u64 << attempt.min(20)));
    let floor = cap / 2;
    Duration::from_millis(floor + jitter(cap - floor + 1))
}

fn jitter(modulus: u64) -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| u64::from(duration.subsec_nanos()) % modulus)
}
