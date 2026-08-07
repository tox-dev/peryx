//! A replica's periodic frontier beacon to its writer.
//!
//! A datacenter writer folds each member's durable frontier into [group readiness](crate::group_readiness),
//! but it never dials a replica: the replica already holds the writer's URL and token to pull the journal,
//! so it also pushes a small authenticated beacon carrying the highest serial it has applied. The writer's
//! [liveness tracker](crate::LivenessTracker) records that frontier and ages the beacon into a suspicion,
//! so a lost replica stops contributing without the writer probing it. Membership is the configured
//! roster, never inferred from the beacon, so a report from an unconfigured node is refused at ingress.

use std::time::Duration;

use reqwest::{Client, Url};

use crate::liveness::HeartbeatReport;
use peryx_storage::meta::MetaStore;

/// How often a replica beats its frontier to the writer. Beats are frequent relative to the writer's
/// suspect and dead windows, so a single dropped beat never ages a healthy replica out.
pub const DEFAULT_BEACON_INTERVAL: Duration = Duration::from_secs(5);

const HEARTBEAT_PATH: &str = "+replication/v1/heartbeat";
const USER_AGENT: &str = concat!("peryx-beacon/", env!("CARGO_PKG_VERSION"));

/// Why a beacon sender could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BeaconError {
    /// The bearer token the beacon authenticates with is empty.
    #[error("beacon token must not be empty")]
    EmptyToken,
    /// The writer URL is not a usable HTTP(S) base.
    #[error("beacon upstream is not a usable HTTP(S) base: {0}")]
    InvalidBase(String),
}

/// A replica's beacon loop: it reports the frontier it has applied to its writer's heartbeat ingest.
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
    /// Build a beacon rooted at the writer `upstream`, reporting `node`'s frontier read from `meta` every
    /// `interval`. `incarnation` rises when this process restarts, so a stale beacon from a prior boot
    /// never supersedes a fresh one.
    ///
    /// # Errors
    /// [`BeaconError::EmptyToken`] for a blank token and [`BeaconError::InvalidBase`] for a URL that is
    /// not a usable HTTP(S) base.
    ///
    /// # Panics
    /// Panics if the HTTP client cannot be built, which a static user agent and a duration timeout over
    /// the guaranteed `rustls` provider never provoke.
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

    /// The report for one beat at `sequence`, carrying the frontier the replica currently holds.
    fn report(&self, sequence: u64) -> HeartbeatReport {
        HeartbeatReport {
            node: self.node.clone(),
            incarnation: self.incarnation,
            sequence,
            applied: Some(self.meta.current_serial().unwrap_or(0)),
        }
    }

    /// Send one beat. A transport failure is returned so the loop can drop it; the next beat re-reports
    /// the frontier, so a missed beat never loses state.
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

    /// Beat forever, waiting `interval` between beats, until the task is dropped at shutdown. A failed
    /// beat is dropped rather than aborting the loop, so a transient writer outage self-heals on the next
    /// beat once the writer returns.
    pub async fn run(self) {
        let mut sequence: u64 = 0;
        loop {
            sequence = sequence.saturating_add(1);
            let _ = self.beat(sequence).await;
            tokio::time::sleep(self.interval).await;
        }
    }
}
