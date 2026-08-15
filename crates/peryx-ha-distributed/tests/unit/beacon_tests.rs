use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use peryx_storage::meta::{MetaError, MetaStore};
use tokio::sync::mpsc;

use crate::support::{TestServer, http_contract};
use crate::{BeaconError, BeaconSender, DEFAULT_BEACON_INTERVAL, LivenessTracker, liveness_router};

const TOKEN: &str = "group-secret";

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

async fn writer() -> (TestServer, Arc<LivenessTracker>) {
    let tracker = Arc::new(LivenessTracker::new(
        ["replica-a".to_owned()],
        Duration::from_secs(10),
        Duration::from_secs(30),
    ));
    let router = Router::new().nest("/mirror", liveness_router(TOKEN, tracker.clone()).unwrap());
    let mut server = TestServer::start(router).await;
    server.url.push_str("mirror");
    (server, tracker)
}

async fn recording_writer() -> (TestServer, mpsc::UnboundedReceiver<()>) {
    async fn record(State(sender): State<mpsc::UnboundedSender<()>>) -> StatusCode {
        sender.send(()).unwrap();
        StatusCode::NO_CONTENT
    }

    let (sender, receiver) = mpsc::unbounded_channel();
    let router = Router::new()
        .route("/+replication/v1/heartbeat", post(record))
        .with_state(sender);
    (TestServer::start(router).await, receiver)
}

#[test]
fn test_configuration_contract() {
    http_contract::assert_configuration(
        |base, token| {
            let dir = tempfile::tempdir().unwrap();
            BeaconSender::new(
                base,
                token,
                "replica-a",
                1,
                seeded_meta(&dir, 0),
                DEFAULT_BEACON_INTERVAL,
            )
            .map(|_| ())
        },
        |error| matches!(error, BeaconError::EmptyToken),
        |error| matches!(error, BeaconError::InvalidBase(_)),
    );
}

#[test]
fn test_new_appends_the_heartbeat_path_to_a_base_without_a_trailing_slash() {
    let dir = tempfile::tempdir().unwrap();
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
    let (server, tracker) = writer().await;
    let dir = tempfile::tempdir().unwrap();
    let beacon = BeaconSender::new(
        &server.url,
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

#[tokio::test(start_paused = true)]
async fn test_run_beats_each_interval_until_it_is_dropped() {
    let (server, mut beats) = recording_writer().await;
    let dir = tempfile::tempdir().unwrap();
    let interval = Duration::from_secs(5);
    let beacon = BeaconSender::new(&server.url, TOKEN, "replica-a", 1, seeded_meta(&dir, 4), interval).unwrap();
    let running = tokio::spawn(beacon.run());

    beats.recv().await.expect("writer stopped");
    tokio::time::advance(interval).await;
    beats.recv().await.expect("writer stopped");

    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
}
