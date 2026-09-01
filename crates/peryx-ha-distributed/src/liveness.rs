//! Ages member heartbeats into readiness and routing hints. Liveness verdicts do not change membership
//! or authority. The configured roster, one observation per member, and the ingest body limit bound
//! retained state.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::http::{PrimaryHttpConfigError, authorized, unauthorized};

/// Caps memory used to decode one heartbeat request.
pub const DEFAULT_MAX_HEARTBEAT_BYTES: usize = 4 * 1024;

pub const DEFAULT_SUSPECT_AFTER: Duration = Duration::from_secs(15);

pub const DEFAULT_DEAD_AFTER: Duration = Duration::from_secs(45);

/// `(incarnation, sequence)` orders reports so the writer can reject replayed or reordered heartbeats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatReport {
    pub node: String,
    pub incarnation: u64,
    pub sequence: u64,
    /// Defaults to `None` so reports from replicas that predate frontier reporting still decode. A
    /// replica whose metadata store cannot be read reports `None` for the same reason it would omit
    /// the field: it has no frontier to offer, and the writer counts it as a silent member rather
    /// than as one that has applied nothing.
    #[serde(default)]
    pub applied: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Beacon {
    incarnation: u64,
    sequence: u64,
}

#[derive(Clone, Copy)]
struct Observation {
    beacon: Beacon,
    at: Instant,
    applied: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Suspicion {
    Alive,
    Suspect,
    Dead,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LivenessRejection {
    #[error("reporting node is not a configured group member")]
    UnknownNode,
    #[error("beacon does not supersede the tracked observation")]
    Stale,
}

/// A configured member with no accepted heartbeat has absent observation fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PeerHealth {
    pub node: String,
    pub suspicion: Suspicion,
    pub incarnation: Option<u64>,
    pub sequence: Option<u64>,
    pub last_seen_seconds: Option<u64>,
    pub applied: Option<u64>,
}

pub struct LivenessTracker {
    members: BTreeSet<String>,
    suspect_after: Duration,
    dead_after: Duration,
    observations: Mutex<BTreeMap<String, Observation>>,
}

impl LivenessTracker {
    #[must_use]
    pub fn new(members: impl IntoIterator<Item = String>, suspect_after: Duration, dead_after: Duration) -> Self {
        Self {
            members: members.into_iter().collect(),
            suspect_after,
            dead_after,
            observations: Mutex::new(BTreeMap::new()),
        }
    }

    /// # Errors
    /// Returns [`LivenessRejection::UnknownNode`] when the reporter is outside the roster and
    /// [`LivenessRejection::Stale`] when the heartbeat does not advance the tracked position.
    pub fn observe(&self, report: &HeartbeatReport, now: Instant) -> Result<(), LivenessRejection> {
        if !self.members.contains(&report.node) {
            return Err(LivenessRejection::UnknownNode);
        }
        let beacon = Beacon {
            incarnation: report.incarnation,
            sequence: report.sequence,
        };
        {
            let mut observations = self
                .observations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(existing) = observations.get(&report.node)
                && beacon <= existing.beacon
            {
                return Err(LivenessRejection::Stale);
            }
            observations.insert(
                report.node.clone(),
                Observation {
                    beacon,
                    at: now,
                    applied: report.applied,
                },
            );
        }
        Ok(())
    }

    /// Returns [`Suspicion::Unknown`] for non-members and members without an accepted heartbeat.
    #[must_use]
    pub fn suspicion(&self, node: &str, now: Instant) -> Suspicion {
        if !self.members.contains(node) {
            return Suspicion::Unknown;
        }
        let observations = self
            .observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.classify(observations.get(node), now)
    }

    /// Orders configured members by node identity for stable serialization.
    #[must_use]
    pub fn summary(&self, now: Instant) -> Vec<PeerHealth> {
        let observations = self
            .observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.members
            .iter()
            .map(|node| {
                let observation = observations.get(node);
                PeerHealth {
                    node: node.clone(),
                    suspicion: self.classify(observation, now),
                    incarnation: observation.map(|observation| observation.beacon.incarnation),
                    sequence: observation.map(|observation| observation.beacon.sequence),
                    last_seen_seconds: observation
                        .map(|observation| now.saturating_duration_since(observation.at).as_secs()),
                    applied: observation.and_then(|observation| observation.applied),
                }
            })
            .collect()
    }

    /// Returns the last reported frontier for alive and suspect members. Dead, unobserved, and
    /// frontier-less members return `None` and do not contribute to durability.
    #[must_use]
    pub fn applied_frontier(&self, node: &str, now: Instant) -> Option<u64> {
        let observation = {
            let observations = self
                .observations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            observations.get(node).copied()
        }?;
        match self.classify(Some(&observation), now) {
            Suspicion::Alive | Suspicion::Suspect => observation.applied,
            Suspicion::Dead | Suspicion::Unknown => None,
        }
    }

    fn classify(&self, observation: Option<&Observation>, now: Instant) -> Suspicion {
        observation.map_or(Suspicion::Unknown, |observation| {
            let age = now.saturating_duration_since(observation.at);
            if age >= self.dead_after {
                Suspicion::Dead
            } else if age >= self.suspect_after {
                Suspicion::Suspect
            } else {
                Suspicion::Alive
            }
        })
    }
}

#[derive(Clone)]
struct LivenessState {
    tracker: Arc<LivenessTracker>,
    token: String,
}

/// # Errors
/// Returns [`PrimaryHttpConfigError::EmptyToken`] when the bearer token is empty.
pub fn liveness_router(
    token: impl Into<String>,
    tracker: Arc<LivenessTracker>,
) -> Result<Router, PrimaryHttpConfigError> {
    let token = token.into();
    if token.is_empty() {
        return Err(PrimaryHttpConfigError::EmptyToken);
    }
    Ok(Router::new()
        .route("/+replication/v1/heartbeat", post(ingest_heartbeat))
        .layer(DefaultBodyLimit::max(DEFAULT_MAX_HEARTBEAT_BYTES))
        .with_state(LivenessState { tracker, token }))
}

async fn ingest_heartbeat(
    State(state): State<LivenessState>,
    headers: HeaderMap,
    report: Result<Json<HeartbeatReport>, JsonRejection>,
) -> Response {
    if !authorized(&headers, &state.token) {
        return unauthorized();
    }
    let report = match report {
        Ok(Json(report)) => report,
        Err(rejection) => return rejection.into_response(),
    };
    match state.tracker.observe(&report, Instant::now()) {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(LivenessRejection::UnknownNode) => {
            (StatusCode::FORBIDDEN, LivenessRejection::UnknownNode.to_string()).into_response()
        }
        Err(LivenessRejection::Stale) => (StatusCode::CONFLICT, LivenessRejection::Stale.to_string()).into_response(),
    }
}
