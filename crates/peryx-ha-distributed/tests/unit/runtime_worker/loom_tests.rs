use std::sync::Arc;
use std::sync::atomic::Ordering;

use loom::sync::mpsc;
use loom::thread;

use crate::runtime_worker::WorkerShared;

#[test]
fn test_worker_slots_admit_one_simultaneous_reservation() {
    loom::model(|| {
        let shared = Arc::new(WorkerShared::new(1, 1));
        let (result_sender, result_receiver) = mpsc::channel();
        let mut releases = Vec::new();
        let mut workers = Vec::new();
        for _ in 0..2 {
            let shared = Arc::clone(&shared);
            let result_sender = result_sender.clone();
            let (release_sender, release_receiver) = mpsc::channel();
            releases.push(release_sender);
            let worker = thread::spawn(move || {
                let slot = shared.reserve();
                result_sender.send(slot.is_some()).unwrap();
                release_receiver.recv().unwrap();
                drop(slot);
            });
            workers.push(worker);
        }

        let admitted = [result_receiver.recv().unwrap(), result_receiver.recv().unwrap()];
        for release in releases {
            release.send(()).unwrap();
        }
        for worker in workers {
            worker.join().unwrap();
        }

        assert_eq!(admitted.into_iter().filter(|admitted| *admitted).count(), 1);
        assert_eq!(shared.rejected.load(Ordering::Relaxed), 1);
        assert_eq!(shared.in_flight.load(Ordering::Relaxed), 0);
        assert!(shared.reserve().is_some());
    });
}
