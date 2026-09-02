use super::*;
use std::collections::HashMap;
use std::time::Duration;

use peryx_ha::{ReplicaPage, ReplicaViewApplier};
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;

use crate::multi_peer::DEFAULT_SET_LIMITS;
use crate::{
    AnalyticsPuller, AvailabilityMetrics, BeaconSender, CapacityLimited, HttpBlobTransport, PeerSet, ReconnectPolicy,
    ReplicaLoopParts, ReplicaMonitor, TransferLimits,
};

#[derive(Default)]
struct Views {
    changed: AtomicUsize,
    frontier: AtomicU64,
}

impl ReplicaViewApplier for Views {
    fn apply(&self, page: ReplicaPage, changed_keys: &[String]) {
        self.changed.fetch_add(changed_keys.len(), Ordering::Relaxed);
        self.frontier.store(page.serial, Ordering::Relaxed);
    }

    fn apply_blob_commit(&self, committed: &[peryx_ha::BlobCommit]) {
        self.changed.fetch_add(
            committed.iter().map(|commit| commit.keys.len()).sum(),
            Ordering::Relaxed,
        );
    }

    fn readable_frontier(&self) -> u64 {
        self.frontier.load(Ordering::Relaxed)
    }

    fn publish_applied_frontier(&self, serial: u64) {
        self.frontier.store(serial, Ordering::Relaxed);
    }
}

fn replica() -> (tempfile::TempDir, MetaStore, ReplicaLoop) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let metadata = PeerSet::new(DEFAULT_SET_LIMITS, ReconnectPolicy::default());
    let transport = HttpBlobTransport::new(
        "http://127.0.0.1:1/",
        "token",
        TransferLimits::default(),
        Duration::from_secs(1),
    )
    .unwrap();
    (
        dir,
        meta.clone(),
        ReplicaLoop::new(ReplicaLoopParts {
            views: Arc::new(Views::default()),
            metadata,
            policy: ReconnectPolicy::default(),
            meta,
            blobs,
            page_size: NonZeroUsize::new(1).unwrap(),
            poll_interval: Duration::from_secs(1),
            monitor: Arc::new(ReplicaMonitor::new(0)),
            metrics: Arc::new(AvailabilityMetrics::default()),
            transport: CapacityLimited::new(transport, NonZeroUsize::new(1).unwrap()),
            local_dc: String::new(),
            delegates: HashMap::new(),
        }),
    )
}

#[test]
fn test_views_apply_pages_and_publish_the_frontier() {
    let views = Views::default();
    views.apply(
        ReplicaPage {
            changes: 2,
            serial: 7,
            primary_serial: 9,
            revocations: Vec::new(),
        },
        &["a".to_owned(), "b".to_owned()],
    );
    assert_eq!(views.changed.load(Ordering::Relaxed), 2);
    views.apply_blob_commit(&[peryx_ha::BlobCommit {
        digest: "sha256:beef".to_owned(),
        keys: vec!["c".to_owned()],
    }]);
    assert_eq!(views.changed.load(Ordering::Relaxed), 3);
    assert_eq!(views.readable_frontier(), 7);
    views.publish_applied_frontier(11);
    assert_eq!(views.readable_frontier(), 11);
}

fn rendered(shared: &WorkerShared) -> String {
    let mut body = String::new();
    shared.write_metrics(&mut body);
    body
}

#[test]
fn test_worker_thread_count_stays_within_the_cap() {
    let count = worker_thread_count();
    assert!((1..=WORKER_THREAD_CAP).contains(&count), "{count}");
}

#[test]
fn test_reserve_hands_out_every_slot_then_applies_backpressure() {
    let shared = Arc::new(WorkerShared::new(2, 2));
    let first = shared.reserve().expect("first slot");
    let second = shared.reserve().expect("second slot");
    assert!(shared.reserve().is_none(), "third reservation must be refused");
    assert_eq!(shared.in_flight.load(Ordering::Relaxed), 2);
    assert_eq!(shared.rejected.load(Ordering::Relaxed), 1);
    drop(first);
    drop(second);
    assert_eq!(shared.in_flight.load(Ordering::Relaxed), 0);
    assert!(shared.reserve().is_some(), "a released slot is reusable");
}

#[test]
fn test_metrics_report_slots_threads_and_counters() {
    let shared = Arc::new(WorkerShared::new(3, 4));
    let _held = shared.reserve().expect("slot");
    let body = rendered(&shared);
    assert!(body.contains("peryx_availability_worker_threads 3\n"), "{body}");
    assert!(body.contains("peryx_availability_worker_slots 4\n"), "{body}");
    assert!(body.contains("peryx_availability_worker_slots_active 1\n"), "{body}");
    assert!(body.contains("peryx_availability_worker_rejected_total 0\n"), "{body}");
    assert!(body.contains("peryx_availability_worker_panics_total 0\n"), "{body}");
}

#[test]
fn test_runtime_starts_and_completes_a_task() {
    let shared = Arc::new(WorkerShared::for_replica());
    let runtime = AvailabilityRuntime::start(Arc::clone(&shared)).expect("runtime");
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let handle = runtime
        .try_spawn(Box::pin(async move {
            started_tx.send(()).expect("receiver waiting");
            let _ = release_rx.await;
        }))
        .expect("slot");
    started_rx.recv().expect("task starts");
    assert_eq!(shared.in_flight.load(Ordering::Relaxed), 1);
    release_tx.send(()).expect("task still waiting");
    runtime.handle.block_on(handle).expect("task joins");
    runtime
        .handle
        .block_on(
            runtime
                .try_spawn(Box::pin(std::future::ready(())))
                .expect("reclaimed slot"),
        )
        .expect("replacement task joins");
    assert_eq!(shared.in_flight.load(Ordering::Relaxed), 0);
    assert!(shared.is_healthy());
}

#[test]
fn test_task_panic_marks_the_domain_unhealthy() {
    let shared = Arc::new(WorkerShared::new(1, 1));
    let runtime = AvailabilityRuntime::start(Arc::clone(&shared)).expect("runtime");
    let handle = runtime.try_spawn(Box::pin(async { panic!("boom") })).expect("slot");
    runtime.handle.block_on(handle).expect("supervisor joins");
    assert!(!shared.is_healthy());
    assert_eq!(shared.panics.load(Ordering::Relaxed), 1);
    assert!(rendered(&shared).contains("peryx_availability_worker_panics_total 1\n"));
}

#[tokio::test]
async fn critical_work_waits_for_activation_and_reports_completion() {
    let (lifecycle, mut failures) = crate::lifecycle::Lifecycle::new();
    let runtime = AvailabilityRuntime::start(Arc::new(WorkerShared::for_replica())).unwrap();
    let (started, mut observed) = tokio::sync::watch::channel(false);
    runtime
        .try_spawn_critical(
            "test",
            Box::pin(async move {
                started.send_replace(true);
            }),
            lifecycle.clone(),
        )
        .unwrap();
    assert!(!*observed.borrow_and_update());

    lifecycle.activate();
    observed.changed().await.unwrap();

    assert_eq!(failures.wait().await, "availability test worker stopped unexpectedly");
}

#[tokio::test]
async fn critical_panic_reports_failure() {
    let (lifecycle, mut failures) = crate::lifecycle::Lifecycle::new();
    lifecycle.activate();
    let runtime = AvailabilityRuntime::start(Arc::new(WorkerShared::for_replica())).unwrap();
    runtime
        .try_spawn_critical("test", Box::pin(async { panic!("boom") }), lifecycle)
        .unwrap();
    assert_eq!(failures.wait().await, "availability test worker panicked");
}

#[tokio::test]
async fn critical_work_cancelled_before_activation_never_runs() {
    let (lifecycle, _) = crate::lifecycle::Lifecycle::new();
    let runtime = AvailabilityRuntime::start(Arc::new(WorkerShared::for_replica())).unwrap();
    let task = runtime
        .try_spawn_critical("test", Box::pin(std::future::pending()), lifecycle.clone())
        .unwrap();

    lifecycle.cancel();
    task.await.unwrap();
}

#[test]
fn test_shutdown_stops_resident_work() {
    struct SendOnDrop(std::sync::mpsc::Sender<()>);
    impl Drop for SendOnDrop {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    let shared = Arc::new(WorkerShared::for_replica());
    let runtime = AvailabilityRuntime::start(Arc::clone(&shared)).expect("runtime");
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
    runtime
        .try_spawn(Box::pin(async move {
            let _stop = SendOnDrop(stopped_tx);
            started_tx.send(()).expect("receiver waiting");
            loop {
                std::future::pending::<()>().await;
            }
        }))
        .expect("slot");
    started_rx.recv().expect("task starts");
    drop(runtime);
    stopped_rx.recv().expect("shutdown drops the resident task");
    assert!(shared.is_healthy(), "a cancelled task is not a panic");
}

#[test]
fn test_join_runtime_thread_reports_a_panic() {
    let thread = std::thread::spawn(|| panic!("injected runtime owner panic"));

    assert_eq!(
        join_runtime_thread(thread).unwrap_err().to_string(),
        "availability runtime thread panicked"
    );
}

#[tokio::test]
async fn test_shutdown_inside_a_runtime_cancels_resident_work() {
    struct Resident {
        started: Option<tokio::sync::oneshot::Sender<()>>,
        stopped: Option<tokio::sync::oneshot::Sender<()>>,
    }
    impl std::future::Future for Resident {
        type Output = ();

        fn poll(mut self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
            self.started.take().unwrap().send(()).unwrap();
            std::task::Poll::Pending
        }
    }
    impl Drop for Resident {
        fn drop(&mut self) {
            self.stopped.take().unwrap().send(()).unwrap();
        }
    }

    let runtime = AvailabilityRuntime::start(Arc::new(WorkerShared::for_replica())).unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (stopped_tx, stopped_rx) = tokio::sync::oneshot::channel();
    runtime
        .try_spawn(Box::pin(Resident {
            started: Some(started_tx),
            stopped: Some(stopped_tx),
        }))
        .expect("slot");
    started_rx.await.unwrap();

    runtime.shutdown().await.unwrap();

    stopped_rx.await.unwrap();
}

#[tokio::test]
async fn test_terminate_workers_cancels_resident_work() {
    struct Resident {
        started: Option<tokio::sync::oneshot::Sender<()>>,
        stopped: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl std::future::Future for Resident {
        type Output = ();

        fn poll(mut self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
            self.started.take().unwrap().send(()).unwrap();
            std::task::Poll::Pending
        }
    }

    impl Drop for Resident {
        fn drop(&mut self) {
            self.stopped.take().unwrap().send(()).unwrap();
        }
    }

    let runtime = AvailabilityRuntime::start(Arc::new(WorkerShared::for_replica())).unwrap();
    let (started, running) = tokio::sync::oneshot::channel();
    let (stopped, joined) = tokio::sync::oneshot::channel();
    runtime
        .try_spawn(Box::pin(Resident {
            started: Some(started),
            stopped: Some(stopped),
        }))
        .unwrap();
    running.await.unwrap();

    runtime.terminate_workers().unwrap();

    joined.await.unwrap();
}

#[tokio::test]
async fn test_drop_aborts_resident_work_without_detached_shutdown() {
    struct Stopped {
        started: Option<tokio::sync::oneshot::Sender<()>>,
        stopped: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl std::future::Future for Stopped {
        type Output = ();

        fn poll(mut self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
            self.started.take().unwrap().send(()).unwrap();
            std::task::Poll::Pending
        }
    }

    impl Drop for Stopped {
        fn drop(&mut self) {
            self.stopped.take().unwrap().send(()).unwrap();
        }
    }

    let runtime = AvailabilityRuntime::start(Arc::new(WorkerShared::for_replica())).unwrap();
    let (started, running) = tokio::sync::oneshot::channel();
    let (stopped, joined) = tokio::sync::oneshot::channel();
    runtime
        .try_spawn(Box::pin(Stopped {
            started: Some(started),
            stopped: Some(stopped),
        }))
        .unwrap();
    running.await.unwrap();

    drop(runtime);

    joined.await.unwrap();
}

#[test]
fn test_drop_does_not_join_a_permanently_blocked_owner() {
    let runtime = AvailabilityRuntime::start(Arc::new(WorkerShared::for_replica())).unwrap();
    let (blocking_started, started) = mpsc::channel();
    let (release, blocked_until_released) = mpsc::channel();
    let blocked = runtime.handle.spawn_blocking(move || {
        blocking_started.send(()).unwrap();
        blocked_until_released.recv().unwrap();
    });
    let runtime_handle = runtime.handle.clone();
    started.recv().unwrap();

    let (dropped, observed) = mpsc::channel();
    let drop_thread = thread::spawn(move || {
        drop(runtime);
        dropped.send(()).unwrap();
    });

    observed.recv_timeout(Duration::from_secs(1)).unwrap();
    release.send(()).unwrap();
    runtime_handle.block_on(blocked).unwrap();
    drop_thread.join().unwrap();
}

#[test]
fn test_saturated_runtime_refuses_further_tasks() {
    let shared = Arc::new(WorkerShared::new(1, 1));
    let runtime = AvailabilityRuntime::start(Arc::clone(&shared)).expect("runtime");
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    let resident = runtime
        .try_spawn(Box::pin(async move {
            let _ = held.await;
        }))
        .expect("slot");
    assert!(
        runtime.try_spawn(Box::pin(std::future::ready(()))).is_none(),
        "a full runtime refuses further work"
    );
    assert_eq!(shared.rejected.load(Ordering::Relaxed), 1);
    release.send(()).expect("resident still waiting");
    runtime.handle.block_on(resident).expect("resident joins");
}

#[test]
fn test_replica_services_start_selected_loops() {
    for (include_optional, expected_workers) in [(false, 1), (true, 3)] {
        let shared = Arc::new(WorkerShared::for_replica());
        let runtime = AvailabilityRuntime::start(Arc::clone(&shared)).unwrap();
        let (_dir, meta, replica) = replica();
        let analytics = include_optional
            .then(|| AnalyticsPuller::new("http://127.0.0.1:1/", "token", meta.analytics(), Duration::from_secs(1)))
            .transpose()
            .unwrap();
        let beacon = include_optional
            .then(|| {
                BeaconSender::new(
                    "http://127.0.0.1:1/",
                    "token",
                    "replica",
                    1,
                    meta,
                    Duration::from_secs(1),
                )
            })
            .transpose()
            .unwrap();

        let (lifecycle, _) = crate::lifecycle::Lifecycle::new();
        lifecycle.activate();
        let runtime = runtime
            .start_replica_services_with_lifecycle(replica, analytics, beacon, lifecycle)
            .unwrap();
        assert_eq!(shared.in_flight.load(Ordering::Relaxed), expected_workers);
        drop(runtime);
    }
}

#[test]
fn test_replica_services_report_the_saturated_worker() {
    for (slots, include_analytics, include_beacon, expected) in [
        (0, false, false, "reserve the replica worker slot"),
        (1, true, false, "reserve the analytics worker slot"),
        (2, true, true, "reserve the beacon worker slot"),
    ] {
        let runtime = AvailabilityRuntime::start(Arc::new(WorkerShared::new(1, slots))).unwrap();
        let (_dir, meta, replica) = replica();
        let analytics = include_analytics
            .then(|| AnalyticsPuller::new("http://127.0.0.1:1/", "token", meta.analytics(), Duration::from_secs(1)))
            .transpose()
            .unwrap();
        let beacon = include_beacon
            .then(|| {
                BeaconSender::new(
                    "http://127.0.0.1:1/",
                    "token",
                    "replica",
                    1,
                    meta,
                    Duration::from_secs(1),
                )
            })
            .transpose()
            .unwrap();
        let (lifecycle, _) = crate::lifecycle::Lifecycle::new();

        assert_eq!(
            runtime
                .start_replica_services_with_lifecycle(replica, analytics, beacon, lifecycle)
                .err()
                .unwrap()
                .to_string(),
            expected
        );
    }
}
