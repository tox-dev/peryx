use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, channel, sync_channel};
use std::time::Duration;

use anyhow::{anyhow, bail};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

const SAMPLE_INTERVAL: Duration = Duration::from_millis(200);

/// Peak resident memory and CPU seconds of one server's process tree during a workload window.
///
/// CPU integrates each process's usage between sample ticks, so work done by children that come
/// and go inside one tick is undercounted; the servers here keep long-lived workers.
pub struct Usage {
    peak_rss: Arc<AtomicU64>,
    cpu_millis: Arc<AtomicU64>,
    stop: Sender<()>,
    handle: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
}

#[cfg(test)]
#[path = "../tests/unit/usage.rs"]
mod tests;

/// What one window cost: `None` when there was no server process to watch.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cost {
    pub cpu_seconds: f64,
    pub peak_rss_bytes: u64,
}

impl Usage {
    /// # Errors
    /// Returns an error when the process cannot be sampled or the sampler thread panics.
    pub fn watch(pid: Option<u32>) -> anyhow::Result<Self> {
        let Some(pid) = pid else {
            let (stop, _) = channel();
            return Ok(Self {
                peak_rss: Arc::new(AtomicU64::new(0)),
                cpu_millis: Arc::new(AtomicU64::new(0)),
                stop,
                handle: None,
            });
        };
        let root = Pid::from_u32(pid);
        let mut system = System::new();
        Self::watch_with(SAMPLE_INTERVAL, move || {
            system.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing().with_cpu().with_memory(),
            );
            process_tree_sample(&system, root)
        })
    }

    /// # Errors
    /// Returns an error when sampling fails or the sampler thread panics.
    pub fn finish(mut self) -> anyhow::Result<Option<Cost>> {
        let Some(handle) = self.handle.take() else {
            return Ok(None);
        };
        let _ = self.stop.send(());
        join_sampler(handle)?;
        Ok(Some(Cost {
            #[expect(clippy::cast_precision_loss, reason = "milliseconds of CPU fit f64 exactly here")]
            cpu_seconds: self.cpu_millis.load(Ordering::Relaxed) as f64 / 1000.0,
            peak_rss_bytes: self.peak_rss.load(Ordering::Relaxed),
        }))
    }

    fn watch_with(
        interval: Duration,
        sampler: impl FnMut() -> anyhow::Result<(u64, u64)> + Send + 'static,
    ) -> anyhow::Result<Self> {
        let peak_rss = Arc::new(AtomicU64::new(0));
        let cpu_millis = Arc::new(AtomicU64::new(0));
        let (stop, stopped) = channel();
        let (notify, started) = sync_channel(1);
        let peak = peak_rss.clone();
        let cpu = cpu_millis.clone();
        let handle = std::thread::spawn(move || sample(sampler, interval, &peak, &cpu, &stopped, &notify));
        match started.recv() {
            Ok(Ok(())) => Ok(Self {
                peak_rss,
                cpu_millis,
                stop,
                handle: Some(handle),
            }),
            Ok(Err(message)) => {
                let _ = handle.join();
                bail!("initial resource sample failed: {message}");
            }
            Err(_) => Err(anyhow!(
                "initial resource sample failed: {}",
                join_sampler(handle).expect_err("the sampler cannot exit cleanly before sending its initial status")
            )),
        }
    }
}

fn join_sampler(handle: std::thread::JoinHandle<anyhow::Result<()>>) -> anyhow::Result<()> {
    match handle.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(anyhow!("resource sampling failed: {error}")),
        Err(_) => bail!("resource sampler thread panicked"),
    }
}

fn sample(
    mut sampler: impl FnMut() -> anyhow::Result<(u64, u64)>,
    interval: Duration,
    peak_rss: &AtomicU64,
    cpu_millis: &AtomicU64,
    stop: &Receiver<()>,
    notify: &SyncSender<Result<(), String>>,
) -> anyhow::Result<()> {
    let initial = match sampler() {
        Ok(sample) => sample,
        Err(error) => {
            let message = error.to_string();
            notify
                .send(Err(message.clone()))
                .expect("the initial status receiver is alive");
            bail!(message);
        }
    };
    record(initial, peak_rss, cpu_millis);
    notify.send(Ok(())).expect("the initial status receiver is alive");
    loop {
        match stop.recv_timeout(interval) {
            Err(RecvTimeoutError::Timeout) => record(sampler()?, peak_rss, cpu_millis),
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn process_tree_sample(system: &System, root: Pid) -> anyhow::Result<(u64, u64)> {
    if system.process(root).is_none() {
        bail!("process {root} is unavailable");
    }
    let tree = tree_of(system, root);
    let rss = tree
        .iter()
        .filter_map(|pid| system.process(*pid))
        .map(sysinfo::Process::memory)
        .sum();
    let usage: f64 = tree
        .iter()
        .filter_map(|pid| system.process(*pid))
        .map(|process| f64::from(process.cpu_usage()))
        .sum();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "usage percent over a 200ms tick is small and non-negative"
    )]
    Ok((rss, (usage / 100.0 * SAMPLE_INTERVAL.as_secs_f64() * 1000.0) as u64))
}

fn record(sample: (u64, u64), peak_rss: &AtomicU64, cpu_millis: &AtomicU64) {
    peak_rss.fetch_max(sample.0, Ordering::Relaxed);
    cpu_millis.fetch_add(sample.1, Ordering::Relaxed);
}

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
