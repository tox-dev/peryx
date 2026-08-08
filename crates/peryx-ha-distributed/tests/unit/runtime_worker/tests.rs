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

struct Views;

impl ReplicaViewApplier for Views {
    fn apply(&self, _page: ReplicaPage, _changed_keys: &[String]) {}

    fn readable_frontier(&self) -> u64 {
        0
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
            views: Arc::new(Views),
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
fn test_runtime_runs_a_task_to_completion() {
    let shared = Arc::new(WorkerShared::for_replica());
    let runtime = AvailabilityRuntime::start(Arc::clone(&shared)).expect("runtime");
    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);
    let handle = runtime
        .try_spawn(Box::pin(async move { flag.store(true, Ordering::Relaxed) }))
        .expect("slot");
    runtime.handle.block_on(handle).expect("task joins");
    assert!(done.load(Ordering::Relaxed));
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
fn test_replica_services_report_initial_saturation() {
    let shared = Arc::new(WorkerShared::new(1, 0));
    let runtime = AvailabilityRuntime::start(shared).unwrap();
    let (_dir, _meta, replica) = replica();

    let error = runtime.start_replica_services(replica, None, None).err().unwrap();

    assert!(error.to_string().contains("reserve the replica loop slot"), "{error}");
}

#[test]
fn test_replica_services_start_selected_loops() {
    for include_optional in [false, true] {
        let runtime = AvailabilityRuntime::start(Arc::new(WorkerShared::for_replica())).unwrap();
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

        drop(runtime.start_replica_services(replica, analytics, beacon).unwrap());
    }
}
