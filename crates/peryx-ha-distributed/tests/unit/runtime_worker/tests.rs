use super::*;

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
