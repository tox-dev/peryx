use std::sync::Arc;
use std::time::{Duration, Instant};

use peryx_storage::meta::{MetaError, MetaStore};

use crate::{BeaconError, BeaconSender, DEFAULT_BEACON_INTERVAL, LivenessTracker, liveness_router};

const TOKEN: &str = "group-secret";

/// A metadata store whose journal has advanced to `serial`, so the beacon reads that frontier.
fn seeded_meta(dir: &tempfile::TempDir, serial: u64) -> MetaStore {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    if serial > 0 {
        let entries: Vec<Vec<u8>> = (0..serial).map(|_| b"beat".to_vec()).collect();
        meta.commit_driver_txn(|_txn| Ok::<((), Vec<Vec<u8>>), MetaError>(((), entries)))
            .unwrap();
    }
    assert_eq!(meta.current_serial().unwrap(), serial);
    meta
}

/// A writer serving the heartbeat ingest on an ephemeral port; returns its base URL and the tracker its
/// ingest records into, so a test can assert what a beacon delivered.
async fn writer() -> (String, Arc<LivenessTracker>) {
    let tracker = Arc::new(LivenessTracker::new(
        ["replica-a".to_owned()],
        Duration::from_secs(10),
        Duration::from_secs(30),
    ));
    let router = liveness_router(TOKEN, tracker.clone()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}/"), tracker)
}

#[test]
fn test_new_rejects_an_empty_token() {
    let dir = tempfile::tempdir().unwrap();
    let result = BeaconSender::new(
        "http://writer/",
        "",
        "replica-a",
        1,
        seeded_meta(&dir, 0),
        DEFAULT_BEACON_INTERVAL,
    );

    assert!(matches!(result, Err(BeaconError::EmptyToken)));
}

#[test]
fn test_new_rejects_an_unparseable_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let result = BeaconSender::new(
        "not a url",
        TOKEN,
        "replica-a",
        1,
        seeded_meta(&dir, 0),
        DEFAULT_BEACON_INTERVAL,
    );

    assert!(matches!(result, Err(BeaconError::InvalidBase(base)) if base == "not a url"));
}

#[test]
fn test_new_rejects_a_non_http_scheme() {
    let dir = tempfile::tempdir().unwrap();
    let result = BeaconSender::new(
        "ftp://writer/",
        TOKEN,
        "replica-a",
        1,
        seeded_meta(&dir, 0),
        DEFAULT_BEACON_INTERVAL,
    );

    assert!(matches!(result, Err(BeaconError::InvalidBase(_))));
}

#[test]
fn test_new_appends_the_heartbeat_path_to_a_base_without_a_trailing_slash() {
    let dir = tempfile::tempdir().unwrap();
    // A base whose path has no trailing slash still resolves; the constructor normalizes it.
    let beacon = BeaconSender::new(
        "http://writer/api",
        TOKEN,
        "replica-a",
        1,
        seeded_meta(&dir, 0),
        DEFAULT_BEACON_INTERVAL,
    );

    assert!(beacon.is_ok());
}

#[tokio::test]
async fn test_beat_reports_the_current_frontier_and_beacon_position() {
    let (address, tracker) = writer().await;
    let dir = tempfile::tempdir().unwrap();
    let beacon = BeaconSender::new(
        &address,
        TOKEN,
        "replica-a",
        7,
        seeded_meta(&dir, 5),
        Duration::from_mins(1),
    )
    .unwrap();

    beacon.beat(3).await.unwrap();

    let now = Instant::now();
    assert_eq!(tracker.applied_frontier("replica-a", now), Some(5));
    let peer = tracker
        .summary(now)
        .into_iter()
        .find(|peer| peer.node == "replica-a")
        .unwrap();
    assert_eq!((peer.incarnation, peer.sequence), (Some(7), Some(3)));
}

#[tokio::test]
async fn test_beat_surfaces_a_transport_failure() {
    let dir = tempfile::tempdir().unwrap();
    // Port 1 refuses the connection, so the send fails rather than hanging.
    let beacon = BeaconSender::new(
        "http://127.0.0.1:1/",
        TOKEN,
        "replica-a",
        1,
        seeded_meta(&dir, 0),
        Duration::from_millis(50),
    )
    .unwrap();

    assert!(beacon.beat(1).await.is_err());
}

#[tokio::test]
async fn test_run_beats_each_interval_until_it_is_dropped() {
    let (address, tracker) = writer().await;
    let dir = tempfile::tempdir().unwrap();
    let interval = Duration::from_millis(5);
    let beacon = BeaconSender::new(&address, TOKEN, "replica-a", 1, seeded_meta(&dir, 4), interval).unwrap();

    // `run` loops forever, so a timeout bounds it. Driving it directly (not spawned) runs the loop body
    // in this task, so its beat and inter-beat wait are exercised deterministically rather than racing a
    // spawned task's scheduling. The first beat fires before any wait, and the loop repeats every
    // interval, so within the deadline it has beaten more than once, then the timeout drops it.
    let outcome = tokio::time::timeout(Duration::from_millis(200), beacon.run()).await;

    assert!(outcome.is_err(), "run loops until dropped, so the timeout elapses");
    assert_eq!(tracker.applied_frontier("replica-a", Instant::now()), Some(4));
    let sequence = tracker
        .summary(Instant::now())
        .into_iter()
        .find(|peer| peer.node == "replica-a")
        .and_then(|peer| peer.sequence);
    assert!(sequence >= Some(2), "the loop beat more than once: {sequence:?}");
}
