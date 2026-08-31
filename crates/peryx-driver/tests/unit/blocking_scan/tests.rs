use std::future::{Future as _, poll_fn};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use tokio::sync::mpsc as async_mpsc;

use super::BlockingScanExecutor;

/// Holds a blocking worker until the test opens it.
///
/// A flag the waiter polls under a condition variable only blocks when the test loses the race to
/// set it, so the wait either runs or does not depending on thread timing. Handing the worker a
/// channel receive instead makes it wait on the release itself: the receive runs on every pass,
/// whether or not the send already landed.
struct Gate {
    open: Sender<()>,
    reached: Mutex<Receiver<()>>,
}

impl Default for Gate {
    fn default() -> Self {
        let (open, reached) = channel();
        Self {
            open,
            reached: Mutex::new(reached),
        }
    }
}

impl Gate {
    fn open(&self) {
        self.open.send(()).unwrap();
    }

    fn wait(&self) {
        self.reached.lock().unwrap().recv().unwrap();
    }
}

#[derive(Default)]
struct WorkerControl {
    page: Gate,
    exit: Gate,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_request_cancellation_holds_capacity_until_the_worker_exits() {
    let executor = BlockingScanExecutor::new(2);
    let controls: Arc<[WorkerControl]> = (0..2).map(|_| WorkerControl::default()).collect();
    let (started_tx, mut started_rx) = async_mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = async_mpsc::unbounded_channel();
    let mut requests = Vec::new();
    for worker in 0..2 {
        let executor = executor.clone();
        let controls = controls.clone();
        let started_tx = started_tx.clone();
        let cancelled_tx = cancelled_tx.clone();
        requests.push(tokio::spawn(async move {
            executor
                .run(move |cancellation| {
                    started_tx.send(worker).unwrap();
                    controls[worker].page.wait();
                    cancelled_tx.send((worker, cancellation.is_cancelled())).unwrap();
                    controls[worker].exit.wait();
                })
                .await
        }));
    }
    for _ in 0..2 {
        started_rx.recv().await.unwrap();
    }

    let cancelled_request = requests.remove(0);
    cancelled_request.abort();
    let cancellation_result = cancelled_request.await;
    controls[0].page.open();
    let cancellation_event = cancelled_rx.recv().await;

    let third_started_tx = started_tx.clone();
    let mut third = Box::pin(executor.run(move |_| third_started_tx.send(2).unwrap()));
    let admitted_before_exit = poll_fn(|cx| Poll::Ready(third.as_mut().poll(cx).is_ready())).await;

    controls[0].exit.open();
    third.await.unwrap();
    let third_start = started_rx.recv().await;

    for control in controls.iter().skip(1) {
        control.page.open();
        control.exit.open();
    }
    for request in requests {
        request.await.unwrap().unwrap();
    }

    assert!(cancellation_result.unwrap_err().is_cancelled());
    assert_eq!(cancellation_event, Some((0, true)));
    assert!(!admitted_before_exit);
    assert_eq!(third_start, Some(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_a_full_executor_refuses_work_until_a_slot_frees() {
    let executor = BlockingScanExecutor::new(1);
    let control = Arc::new(WorkerControl::default());
    let (started_tx, mut started_rx) = async_mpsc::unbounded_channel();
    let holder = tokio::spawn({
        let executor = executor.clone();
        let control = control.clone();
        async move {
            executor
                .try_run(move |_| {
                    started_tx.send(()).unwrap();
                    control.page.wait();
                })
                .await
        }
    });
    started_rx.recv().await.unwrap();

    let refused = executor.try_run(|_| ()).await.is_none();

    control.page.open();
    holder.await.unwrap().unwrap().unwrap();
    let readmitted = executor.try_run(|_| ()).await.is_some();

    assert_eq!((refused, readmitted), (true, true));
}
