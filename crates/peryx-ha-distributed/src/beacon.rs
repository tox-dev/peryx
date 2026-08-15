//! Replicas push their durable frontier to the writer for
//! [group readiness](crate::group_readiness). Heartbeats report health but do not establish membership;
//! the writer rejects nodes outside its configured roster.

use std::time::Duration;

use reqwest::{Client, Url};

use crate::liveness::HeartbeatReport;
use peryx_storage::meta::MetaStore;

/// With the default liveness windows, one dropped beat does not age out a healthy replica.
pub const DEFAULT_BEACON_INTERVAL: Duration = Duration::from_secs(5);

const HEARTBEAT_PATH: &str = "+replication/v1/heartbeat";
const USER_AGENT: &str = concat!("peryx-beacon/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BeaconError {
    #[error("beacon token must not be empty")]
    EmptyToken,
    #[error("beacon upstream is not a usable HTTP(S) base: {0}")]
    InvalidBase(String),
}

pub struct BeaconSender {
    http: Client,
    endpoint: Url,
    token: String,
    node: String,
    incarnation: u64,
    meta: MetaStore,
    interval: Duration,
}

impl BeaconSender {
    /// Callers must increase `incarnation` after a process restart so heartbeats from the prior process
    /// cannot supersede new ones.
    ///
    /// # Errors
    /// Returns [`BeaconError::EmptyToken`] for a blank token and [`BeaconError::InvalidBase`] for an
    /// unusable HTTP(S) base.
    ///
    /// # Panics
    /// Panics if reqwest rejects the static user agent or duration timeout.
    pub fn new(
        upstream: &str,
        token: impl Into<String>,
        node: impl Into<String>,
        incarnation: u64,
        meta: MetaStore,
        interval: Duration,
    ) -> Result<Self, BeaconError> {
        let token = token.into();
        if token.is_empty() {
            return Err(BeaconError::EmptyToken);
        }
        let Ok(mut base) = Url::parse(upstream) else {
            return Err(BeaconError::InvalidBase(upstream.to_owned()));
        };
        if !matches!(base.scheme(), "http" | "https") || base.cannot_be_a_base() {
            return Err(BeaconError::InvalidBase(upstream.to_owned()));
        }
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        base.set_query(None);
        base.set_fragment(None);
        base.set_path(&format!("{}{HEARTBEAT_PATH}", base.path()));
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(interval)
            .build()
            .expect("a reqwest client with a static user agent and a duration timeout always builds");
        Ok(Self {
            http,
            endpoint: base,
            token,
            node: node.into(),
            incarnation,
            meta,
            interval,
        })
    }

    fn report(&self, sequence: u64) -> HeartbeatReport {
        HeartbeatReport {
            node: self.node.clone(),
            incarnation: self.incarnation,
            sequence,
            applied: Some(self.meta.current_serial().unwrap_or(0)),
        }
    }

    /// Returns transport failures without losing frontier state; the next beat reports it again.
    pub(crate) async fn beat(&self, sequence: u64) -> Result<(), reqwest::Error> {
        let body = serde_json::to_vec(&self.report(sequence)).expect("a heartbeat report serializes");
        self.http
            .post(self.endpoint.clone())
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await?;
        Ok(())
    }

    /// Runs until cancellation and discards failed beats so a writer outage does not stop reporting.
    pub async fn run(self) {
        let mut sequence: u64 = 0;
        loop {
            sequence = sequence.saturating_add(1);
            let _ = self.beat(sequence).await;
            tokio::time::sleep(self.interval).await;
        }
    }
}
