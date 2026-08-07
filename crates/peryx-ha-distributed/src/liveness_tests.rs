use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

use crate::{
    DEFAULT_MAX_HEARTBEAT_BYTES, HeartbeatReport, LivenessRejection, LivenessTracker, Suspicion, liveness_router,
};

const TOKEN: &str = "group-secret";

fn beacon(node: &str, incarnation: u64, sequence: u64) -> HeartbeatReport {
    HeartbeatReport {
        node: node.to_owned(),
        incarnation,
        sequence,
        applied: None,
    }
}

fn beacon_at(node: &str, incarnation: u64, sequence: u64, applied: u64) -> HeartbeatReport {
    HeartbeatReport {
        applied: Some(applied),
        ..beacon(node, incarnation, sequence)
    }
}

fn tracker() -> LivenessTracker {
    LivenessTracker::new(
        ["replica-a".to_owned(), "replica-b".to_owned()],
        Duration::from_secs(10),
        Duration::from_secs(30),
    )
}

#[test]
fn test_fresh_beacon_reports_alive() {
    let tracker = tracker();
    let now = Instant::now();

    tracker.observe(&beacon("replica-a", 1, 1), now).unwrap();

    assert_eq!(tracker.suspicion("replica-a", now), Suspicion::Alive);
}

#[test]
fn test_observation_ages_through_suspect_into_dead() {
    let tracker = tracker();
    let now = Instant::now();
    tracker.observe(&beacon("replica-a", 1, 1), now).unwrap();

    assert_eq!(
        tracker.suspicion("replica-a", now + Duration::from_secs(5)),
        Suspicion::Alive
    );
    assert_eq!(
        tracker.suspicion("replica-a", now + Duration::from_secs(20)),
        Suspicion::Suspect
    );
    assert_eq!(
        tracker.suspicion("replica-a", now + Duration::from_secs(40)),
        Suspicion::Dead
    );
}

#[test]
fn test_configured_member_without_a_beacon_is_unknown() {
    let tracker = tracker();

    assert_eq!(tracker.suspicion("replica-b", Instant::now()), Suspicion::Unknown);
}

#[test]
fn test_non_member_is_unknown() {
    let tracker = tracker();

    assert_eq!(tracker.suspicion("stranger", Instant::now()), Suspicion::Unknown);
}

#[test]
fn test_report_from_a_non_member_is_rejected() {
    let tracker = tracker();

    let error = tracker.observe(&beacon("stranger", 1, 1), Instant::now()).unwrap_err();

    assert_eq!(error, LivenessRejection::UnknownNode);
    assert_eq!(error.to_string(), "reporting node is not a configured group member");
}

#[test]
fn test_a_replayed_or_reordered_beacon_is_stale() {
    let tracker = tracker();
    let now = Instant::now();
    tracker.observe(&beacon("replica-a", 1, 5), now).unwrap();

    let replay = tracker.observe(&beacon("replica-a", 1, 5), now).unwrap_err();
    let reorder = tracker.observe(&beacon("replica-a", 1, 4), now).unwrap_err();

    assert_eq!(replay, LivenessRejection::Stale);
    assert_eq!(reorder, LivenessRejection::Stale);
    assert_eq!(replay.to_string(), "beacon does not supersede the tracked observation");
}

#[test]
fn test_a_higher_incarnation_supersedes_a_lower_sequence() {
    let tracker = tracker();
    let now = Instant::now();
    tracker.observe(&beacon("replica-a", 1, 9), now).unwrap();

    tracker
        .observe(&beacon("replica-a", 2, 1), now + Duration::from_secs(1))
        .unwrap();

    let summary = tracker.summary(now + Duration::from_secs(1));
    let peer = summary.iter().find(|peer| peer.node == "replica-a").unwrap();
    assert_eq!(peer.incarnation, Some(2));
    assert_eq!(peer.sequence, Some(1));
}

#[test]
fn test_summary_covers_every_member_in_order() {
    let tracker = tracker();
    let now = Instant::now();
    tracker.observe(&beacon("replica-a", 1, 1), now).unwrap();

    let summary = tracker.summary(now + Duration::from_secs(1));

    assert_eq!(summary.len(), 2);
    assert_eq!(summary[0].node, "replica-a");
    assert_eq!(summary[0].suspicion, Suspicion::Alive);
    assert_eq!(summary[0].last_seen_seconds, Some(1));
    assert_eq!(summary[1].node, "replica-b");
    assert_eq!(summary[1].suspicion, Suspicion::Unknown);
    assert_eq!(summary[1].incarnation, None);
    assert_eq!(summary[1].last_seen_seconds, None);
}

#[test]
fn test_asymmetric_partition_holds_divergent_suspicions() {
    let west = tracker();
    let east = tracker();
    let now = Instant::now();
    let report = beacon("replica-a", 1, 1);
    west.observe(&report, now).unwrap();
    east.observe(&report, now).unwrap();

    let later = now + Duration::from_secs(20);
    west.observe(&beacon("replica-a", 1, 2), later).unwrap();

    assert_eq!(west.suspicion("replica-a", later), Suspicion::Alive);
    assert_eq!(east.suspicion("replica-a", later), Suspicion::Suspect);
}

fn router() -> axum::Router {
    liveness_router(
        TOKEN,
        Arc::new(LivenessTracker::new(
            ["replica-a".to_owned()],
            Duration::from_secs(10),
            Duration::from_secs(30),
        )),
    )
    .unwrap()
}

async fn post(router: &axum::Router, credentials: Option<&str>, body: Body) -> StatusCode {
    let mut request = Request::post("/+replication/v1/heartbeat").header(header::CONTENT_TYPE, "application/json");
    if let Some(credentials) = credentials {
        request = request.header(header::AUTHORIZATION, format!("Bearer {credentials}"));
    }
    router
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap()
        .status()
}

fn json_body(report: &HeartbeatReport) -> Body {
    Body::from(serde_json::to_vec(report).unwrap())
}

#[test]
fn test_an_empty_token_router_is_rejected() {
    let error = liveness_router(
        "",
        Arc::new(LivenessTracker::new([], Duration::from_secs(1), Duration::from_secs(2))),
    )
    .unwrap_err();

    assert_eq!(error, crate::PrimaryHttpConfigError::EmptyToken);
}

#[tokio::test]
async fn test_ingest_accepts_an_authenticated_beacon() {
    let router = router();

    let status = post(&router, Some(TOKEN), json_body(&beacon("replica-a", 1, 1))).await;

    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_ingest_rejects_a_missing_credential() {
    let router = router();

    let status = post(&router, None, json_body(&beacon("replica-a", 1, 1))).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_ingest_rejects_a_wrong_credential() {
    let router = router();

    let status = post(&router, Some("wrong"), json_body(&beacon("replica-a", 1, 1))).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_ingest_rejects_a_non_member() {
    let router = router();

    let status = post(&router, Some(TOKEN), json_body(&beacon("stranger", 1, 1))).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_ingest_rejects_a_stale_beacon() {
    let router = router();
    assert_eq!(
        post(&router, Some(TOKEN), json_body(&beacon("replica-a", 1, 5))).await,
        StatusCode::ACCEPTED
    );

    let status = post(&router, Some(TOKEN), json_body(&beacon("replica-a", 1, 5))).await;

    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_ingest_rejects_a_malformed_body() {
    let router = router();

    let status = post(&router, Some(TOKEN), Body::from("not json")).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_ingest_rejects_an_oversized_body() {
    let router = router();
    let node = "x".repeat(DEFAULT_MAX_HEARTBEAT_BYTES + 1);

    let status = post(&router, Some(TOKEN), json_body(&beacon(&node, 1, 1))).await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn test_ingest_response_body_names_the_rejection() {
    let router = router();
    let response = router
        .oneshot(
            Request::post("/+replication/v1/heartbeat")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(json_body(&beacon("stranger", 1, 1)))
                .unwrap(),
        )
        .await
        .unwrap();

    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, "reporting node is not a configured group member".as_bytes());
}

#[test]
fn test_applied_frontier_reports_the_last_beat() {
    let tracker = tracker();
    let now = Instant::now();

    tracker.observe(&beacon_at("replica-a", 1, 1, 42), now).unwrap();

    assert_eq!(tracker.applied_frontier("replica-a", now), Some(42));
}

#[test]
fn test_applied_frontier_survives_into_the_suspect_window() {
    let tracker = tracker();
    let now = Instant::now();
    tracker.observe(&beacon_at("replica-a", 1, 1, 7), now).unwrap();

    // Suspect (past the 10s suspect window, before the 30s dead window) still counts its last frontier.
    assert_eq!(
        tracker.applied_frontier("replica-a", now + Duration::from_secs(20)),
        Some(7),
    );
}

#[test]
fn test_applied_frontier_drops_once_the_beacon_is_dead() {
    let tracker = tracker();
    let now = Instant::now();
    tracker.observe(&beacon_at("replica-a", 1, 1, 7), now).unwrap();

    // Past the dead window a silent member holds nothing the group can guarantee.
    assert_eq!(
        tracker.applied_frontier("replica-a", now + Duration::from_secs(31)),
        None
    );
}

#[test]
fn test_applied_frontier_is_none_for_an_unobserved_or_unconfigured_node() {
    let tracker = tracker();
    let now = Instant::now();

    assert_eq!(tracker.applied_frontier("replica-b", now), None);
    assert_eq!(tracker.applied_frontier("stranger", now), None);
}

#[test]
fn test_applied_frontier_is_none_for_a_beat_that_reports_no_frontier() {
    let tracker = tracker();
    let now = Instant::now();
    tracker.observe(&beacon("replica-a", 1, 1), now).unwrap();

    assert_eq!(tracker.applied_frontier("replica-a", now), None);
}

#[test]
fn test_summary_carries_the_reported_frontier() {
    let tracker = tracker();
    let now = Instant::now();
    tracker.observe(&beacon_at("replica-a", 1, 1, 9), now).unwrap();

    let summary = tracker.summary(now);
    let peer = summary.iter().find(|peer| peer.node == "replica-a").unwrap();
    assert_eq!(peer.applied, Some(9));
    let unheard = summary.iter().find(|peer| peer.node == "replica-b").unwrap();
    assert_eq!(unheard.applied, None);
}
