//! Server resource accounting: sample a process tree while a workload runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sysinfo::{Pid, ProcessesToUpdate, System};

/// Peak resident memory and CPU seconds of one server's process tree during a workload window.
///
/// CPU integrates each process's usage between sample ticks, so work done by children that come
/// and go inside one tick is undercounted; the servers here keep long-lived workers.
pub struct Usage {
    peak_rss: Arc<AtomicU64>,
    cpu_millis: Arc<AtomicU64>,
    stop: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// What one window cost: `None` when there was no server process to watch.
#[derive(Clone, Copy)]
pub struct Cost {
    pub cpu_seconds: f64,
    pub peak_rss_bytes: u64,
}

impl Usage {
    /// Start sampling `pid`'s tree; `None` (the direct baseline) records nothing.
    pub fn watch(pid: Option<u32>) -> Self {
        let peak_rss = Arc::new(AtomicU64::new(0));
        let cpu_millis = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicU64::new(0));
        let handle = pid.map(|pid| {
            let peak = peak_rss.clone();
            let cpu = cpu_millis.clone();
            let stopped = stop.clone();
            std::thread::spawn(move || sample(Pid::from_u32(pid), &peak, &cpu, &stopped))
        });
        Self {
            peak_rss,
            cpu_millis,
            stop,
            handle,
        }
    }

    /// Stop sampling and report the window's cost.
    pub fn finish(mut self) -> Option<Cost> {
        let handle = self.handle.take()?;
        self.stop.store(1, Ordering::Relaxed);
        let _ = handle.join();
        Some(Cost {
            #[expect(clippy::cast_precision_loss, reason = "milliseconds of CPU fit f64 exactly here")]
            cpu_seconds: self.cpu_millis.load(Ordering::Relaxed) as f64 / 1000.0,
            peak_rss_bytes: self.peak_rss.load(Ordering::Relaxed),
        })
    }
}

fn sample(root: Pid, peak_rss: &AtomicU64, cpu_millis: &AtomicU64, stop: &AtomicU64) {
    let mut system = System::new();
    let interval = Duration::from_millis(200);
    while stop.load(Ordering::Relaxed) == 0 {
        system.refresh_processes(ProcessesToUpdate::All, true);
        let tree = tree_of(&system, root);
        let rss: u64 = tree
            .iter()
            .filter_map(|pid| system.process(*pid))
            .map(sysinfo::Process::memory)
            .sum();
        let usage: f64 = tree
            .iter()
            .filter_map(|pid| system.process(*pid))
            .map(|process| f64::from(process.cpu_usage()))
            .sum();
        peak_rss.fetch_max(rss, Ordering::Relaxed);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "usage percent over a 200ms tick is small and non-negative"
        )]
        cpu_millis.fetch_add(
            (usage / 100.0 * interval.as_secs_f64() * 1000.0) as u64,
            Ordering::Relaxed,
        );
        std::thread::sleep(interval);
    }
}

/// The pids rooted at `root`, walking parent links across the whole process table.
fn tree_of(system: &System, root: Pid) -> Vec<Pid> {
    system
        .processes()
        .keys()
        .filter(|&&pid| {
            let mut cursor = pid;
            loop {
                if cursor == root {
                    return true;
                }
                match system.process(cursor).and_then(sysinfo::Process::parent) {
                    Some(parent) if parent != cursor => cursor = parent,
                    _ => return false,
                }
            }
        })
        .copied()
        .collect()
}
