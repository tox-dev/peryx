//! Bounded peer-liveness tracking for a static datacenter replication group.
//!
//! Members beacon their health to the group's writer, which ages each observation into an `alive`,
//! `suspect`, or `dead` verdict. The verdict feeds readiness and routing hints only: a missed beacon
//! never edits the configured roster, evicts a replica, or transfers authority, so an asymmetric
//! partition can leave two observers holding different suspicions without either mutating membership.
//! The state a tracker keeps is bounded by the fixed roster, one superseding observation per member,
//! and the ingest body cap, so a hostile or looping reporter cannot grow it.

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

/// The largest heartbeat body the ingest endpoint decodes, bounding per-request memory.
pub const DEFAULT_MAX_HEARTBEAT_BYTES: usize = 4 * 1024;

/// How long an observation stays `alive` before the writer treats the peer as `suspect`.
pub const DEFAULT_SUSPECT_AFTER: Duration = Duration::from_secs(15);

/// How long an observation stays trackable before the writer treats the peer as `dead`.
pub const DEFAULT_DEAD_AFTER: Duration = Duration::from_secs(45);

/// A peer's self-reported liveness beacon.
///
/// `incarnation` rises when a peer restarts and `sequence` rises with each beat, so the pair totally
/// orders a peer's beacons and lets the writer drop a replayed or reordered report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatReport {
    pub node: String,
    pub incarnation: u64,
    pub sequence: u64,
    /// The highest metadata serial this member has durably applied at the beat, so the writer can
    /// aggregate the group's durable frontier without polling each replica. Defaults to `None` so a
    /// beacon from an older replica that reports no frontier still decodes.
    #[serde(default)]
    pub applied: Option<u64>,
}

/// The monotonic position of one beacon, ordered first by incarnation and then by sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Beacon {
    incarnation: u64,
    sequence: u64,
}

#[derive(Clone, Copy)]
struct Observation {
    beacon: Beacon,
    at: Instant,
    /// The frontier the peer reported on this beat, retained so the writer's group-readiness fold reads
    /// each member's durable serial from its latest accepted beacon.
    applied: Option<u64>,
}

/// A tracker's verdict about one configured peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Suspicion {
    /// A recent beacon places the peer within the alive window.
    Alive,
    /// The most recent beacon has aged past the suspect window but not the dead window.
    Suspect,
    /// The most recent beacon has aged past the dead window.
    Dead,
    /// The peer is configured but has sent no beacon this tracker has accepted.
    Unknown,
}

/// Why a tracker refused a heartbeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LivenessRejection {
    /// The reporting node is not a configured group member.
    #[error("reporting node is not a configured group member")]
    UnknownNode,
    /// The beacon does not supersede the observation already held for the node.
    #[error("beacon does not supersede the tracked observation")]
    Stale,
}

/// One member's aged-out liveness, serialized into the writer's availability document as a routing
/// hint. Absent fields mark a member the tracker has never heard from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PeerHealth {
    pub node: String,
    pub suspicion: Suspicion,
    pub incarnation: Option<u64>,
    pub sequence: Option<u64>,
    pub last_seen_seconds: Option<u64>,
    /// The frontier the peer last reported, for an operator reading how far each replica has caught up.
    pub applied: Option<u64>,
}

/// Ages self-reported beacons from the configured replica roster into per-peer suspicion.
pub struct LivenessTracker {
    members: BTreeSet<String>,
    suspect_after: Duration,
    dead_after: Duration,
    observations: Mutex<BTreeMap<String, Observation>>,
}

impl LivenessTracker {
    /// Track the given members, aging an accepted beacon through `suspect_after` and then
    /// `dead_after`.
    #[must_use]
    pub fn new(members: impl IntoIterator<Item = String>, suspect_after: Duration, dead_after: Duration) -> Self {
        Self {
            members: members.into_iter().collect(),
            suspect_after,
            dead_after,
            observations: Mutex::new(BTreeMap::new()),
        }
    }

    /// Record a beacon, or reject a report from an unconfigured node or one that does not supersede
    /// the observation already held.
    ///
    /// # Errors
    /// Returns [`LivenessRejection::UnknownNode`] when the reporter is outside the roster and
    /// [`LivenessRejection::Stale`] when the beacon does not advance the tracked position.
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

    /// The suspicion held for one node, `Unknown` for a non-member or a member with no accepted
    /// beacon.
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

    /// The suspicion for every configured member, ordered by node identity for a stable document.
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

    /// The frontier `node` last reported, counted only while its beacon is still within the dead window.
    ///
    /// A member aged out to [`Suspicion::Dead`], one never observed, or one reporting no frontier holds
    /// nothing the group can guarantee, so it reads `None` and the readiness fold treats it as a member
    /// that is not currently contributing to durability. A live or merely suspect member contributes the
    /// serial it last confirmed.
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

/// Build the bearer-authenticated heartbeat ingest the group writer mounts alongside its primary
/// routes.
///
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
