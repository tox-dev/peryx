use std::future::{Future as _, poll_fn};
use std::sync::{Arc, Condvar, Mutex};
use std::task::Poll;

use tokio::sync::mpsc;

use super::BlockingScanExecutor;

#[derive(Default)]
struct WorkerState {
    page_released: bool,
    exit_released: bool,
}

#[derive(Default)]
struct WorkerControl {
    state: Mutex<WorkerState>,
    changed: Condvar,
}

impl WorkerControl {
    fn release_page(&self) {
        self.state.lock().unwrap().page_released = true;
        self.changed.notify_all();
    }

    fn release_exit(&self) {
        self.state.lock().unwrap().exit_released = true;
        self.changed.notify_all();
    }

    fn wait_for_page(&self) {
        let mut state = self.state.lock().unwrap();
        while !state.page_released {
            state = self.changed.wait(state).unwrap();
        }
        drop(state);
    }

    fn wait_for_exit(&self) {
        let mut state = self.state.lock().unwrap();
        while !state.exit_released {
            state = self.changed.wait(state).unwrap();
        }
        drop(state);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_request_cancellation_holds_capacity_until_the_worker_exits() {
    let executor = BlockingScanExecutor::new(2);
    let controls: Arc<[WorkerControl]> = (0..2).map(|_| WorkerControl::default()).collect();
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
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
                    controls[worker].wait_for_page();
                    cancelled_tx.send((worker, cancellation.is_cancelled())).unwrap();
                    controls[worker].wait_for_exit();
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
    controls[0].release_page();
    let cancellation_event = cancelled_rx.recv().await;

    let third_started_tx = started_tx.clone();
    let mut third = Box::pin(executor.run(move |_| third_started_tx.send(2).unwrap()));
    let admitted_before_exit = poll_fn(|cx| Poll::Ready(third.as_mut().poll(cx).is_ready())).await;

    controls[0].release_exit();
    third.await.unwrap();
    let third_start = started_rx.recv().await;

    for control in controls.iter().skip(1) {
        control.release_page();
        control.release_exit();
    }
    for request in requests {
        request.await.unwrap().unwrap();
    }

    assert!(cancellation_result.unwrap_err().is_cancelled());
    assert_eq!(cancellation_event, Some((0, true)));
    assert!(!admitted_before_exit);
    assert_eq!(third_start, Some(2));
}
