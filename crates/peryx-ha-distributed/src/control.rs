use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use peryx_ha::{CommandReceipt, ControlCommand, ControlError, ControlExecutor, ControlMetrics, MembershipControl};
use serde::Serialize;
use tokio::sync::{Semaphore, watch};

use peryx_core::Clock;

const MAX_CONCURRENT_COMMANDS: usize = 4;

const RETAINED: usize = 256;

/// Monotonic elapsed time since an arbitrary origin.
pub type DurationSource = Arc<dyn Fn() -> Duration + Send + Sync>;

#[must_use]
pub fn plan_voter_roster(current: &BTreeSet<u64>, add: Option<u64>, remove: Option<u64>) -> BTreeSet<u64> {
    let mut roster = current.clone();
    if let Some(id) = add {
        roster.insert(id);
    }
    if let Some(id) = remove {
        roster.remove(&id);
    }
    roster
}

/// Excludes request bodies so addresses and tokens do not enter the audit log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditRecord {
    pub actor: String,
    pub command: &'static str,
    pub target: String,
    pub result: &'static str,
    pub term: Option<u64>,
    pub index: Option<u64>,
    pub old_voters: Vec<String>,
    pub new_voters: Vec<String>,
}

impl AuditRecord {
    fn committed(actor: &str, command: &ControlCommand, receipt: &CommandReceipt) -> Self {
        Self {
            actor: actor.to_owned(),
            command: command.kind(),
            target: command.target().to_owned(),
            result: receipt.outcome.as_str(),
            term: Some(receipt.term),
            index: Some(receipt.index),
            old_voters: receipt.old_voters.clone(),
            new_voters: receipt.new_voters.clone(),
        }
    }

    fn replayed(actor: &str, command: &ControlCommand, receipt: &CommandReceipt) -> Self {
        Self {
            result: "replayed",
            ..Self::committed(actor, command, receipt)
        }
    }

    fn failed(actor: &str, command: &ControlCommand, error: &ControlError) -> Self {
        Self {
            actor: actor.to_owned(),
            command: command.kind(),
            target: command.target().to_owned(),
            result: error.kind(),
            term: None,
            index: None,
            old_voters: Vec::new(),
            new_voters: Vec::new(),
        }
    }

    fn emit(&self, timestamp_unix: i64) {
        tracing::info!(
            timestamp_unix,
            actor = %self.actor,
            command = self.command,
            target = %self.target,
            result = self.result,
            term = ?self.term,
            index = ?self.index,
            old_voters = ?self.old_voters,
            new_voters = ?self.new_voters,
            "availability control command",
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CommandMetrics {
    /// Commands completed since start, excluding replays.
    pub completed: u64,
    /// The median command latency over the retained window, in milliseconds.
    pub p50_ms: i64,
    /// The 99th-percentile command latency over the retained window, in milliseconds.
    pub p99_ms: i64,
}

/// Holds the canonical command rather than a digest of it, so a replay compares what was asked for
/// instead of a hash whose output no toolchain promises to keep stable.
struct KeyEntry {
    key: String,
    command: ControlCommand,
    state: KeyState,
}

enum KeyState {
    /// Version tracking prevents a retry from missing an owner drop after cloning under the lock.
    Pending(watch::Receiver<()>),
    Done(CommandReceipt),
}

enum Claim {
    Execute(watch::Sender<()>),
    Replay(CommandReceipt),
    Wait(watch::Receiver<()>),
    Conflict,
}

struct ClaimGuard<'a> {
    plane: &'a ControlPlane,
    key: &'a str,
    sender: Option<watch::Sender<()>>,
}

impl ClaimGuard<'_> {
    fn settle(mut self, result: &Result<CommandReceipt, ControlError>) {
        self.plane.settle(self.key, result);
        self.sender.take();
    }
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        if self.sender.is_some() {
            self.plane.abandon(self.key);
        }
    }
}

#[derive(Default)]
struct History {
    receipts: VecDeque<KeyEntry>,
    latencies: VecDeque<i64>,
}

pub struct ControlPlane {
    control: Arc<dyn MembershipControl>,
    unix_clock: Clock,
    duration_source: DurationSource,
    permits: Semaphore,
    retained: usize,
    completed: std::sync::atomic::AtomicU64,
    history: Mutex<History>,
}

impl ControlPlane {
    #[must_use]
    pub fn new(control: Arc<dyn MembershipControl>, unix_clock: Clock) -> Self {
        let origin = Instant::now();
        Self::with_duration_source(control, unix_clock, Arc::new(move || origin.elapsed()))
    }

    #[must_use]
    pub fn with_duration_source(
        control: Arc<dyn MembershipControl>,
        unix_clock: Clock,
        duration_source: DurationSource,
    ) -> Self {
        Self::with_limits(control, unix_clock, duration_source, MAX_CONCURRENT_COMMANDS, RETAINED)
    }

    fn with_limits(
        control: Arc<dyn MembershipControl>,
        unix_clock: Clock,
        duration_source: DurationSource,
        concurrency: usize,
        retained: usize,
    ) -> Self {
        Self {
            control,
            unix_clock,
            duration_source,
            permits: Semaphore::new(concurrency),
            retained,
            completed: std::sync::atomic::AtomicU64::new(0),
            history: Mutex::new(History::default()),
        }
    }

    /// Binds `key` to the command before submission. Concurrent retries wait and replay the committed
    /// receipt; reuse with a different command fails. This binding only spares one process a round trip:
    /// the replicated window behind [`MembershipControl`] is what survives restart and failover.
    ///
    /// # Errors
    /// Returns [`ControlError::KeyReuse`] when `key` was already claimed for a different command,
    /// [`ControlError::Overloaded`] when the concurrency bound is saturated, or the [`ControlError`] the
    /// submission produced.
    pub async fn execute(
        &self,
        actor: &str,
        key: Option<&str>,
        command: ControlCommand,
    ) -> Result<CommandReceipt, ControlError> {
        let Some(key) = key else {
            return self.run(actor, None, &command).await;
        };
        loop {
            match self.claim(key, &command) {
                Claim::Replay(receipt) => {
                    AuditRecord::replayed(actor, &command, &receipt).emit((self.unix_clock)());
                    return Ok(receipt);
                }
                Claim::Conflict => {
                    let error = ControlError::KeyReuse;
                    AuditRecord::failed(actor, &command, &error).emit((self.unix_clock)());
                    return Err(error);
                }
                Claim::Execute(sender) => {
                    let claim = ClaimGuard {
                        plane: self,
                        key,
                        sender: Some(sender),
                    };
                    let result = self.run(actor, Some(key), &command).await;
                    claim.settle(&result);
                    return result;
                }
                Claim::Wait(mut receiver) => {
                    let _ = receiver.changed().await;
                }
            }
        }
    }

    /// A receipt the replicated window replayed is audited as a replay and left out of the completion
    /// count, so a retry answered after a failover is not reported as a second command.
    async fn run(
        &self,
        actor: &str,
        key: Option<&str>,
        command: &ControlCommand,
    ) -> Result<CommandReceipt, ControlError> {
        let Ok(_permit) = self.permits.try_acquire() else {
            let error = ControlError::Overloaded;
            AuditRecord::failed(actor, command, &error).emit((self.unix_clock)());
            return Err(error);
        };
        let timestamp_unix = (self.unix_clock)();
        let started = (self.duration_source)();
        let result = self.control.submit(key, command.clone()).await;
        let elapsed = (self.duration_source)().saturating_sub(started);
        let replayed = matches!(&result, Ok(commit) if commit.replayed);
        if !replayed {
            self.record(i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX));
        }
        match &result {
            Ok(commit) if replayed => AuditRecord::replayed(actor, command, &commit.receipt).emit(timestamp_unix),
            Ok(commit) => AuditRecord::committed(actor, command, &commit.receipt).emit(timestamp_unix),
            Err(error) => AuditRecord::failed(actor, command, error).emit(timestamp_unix),
        }
        result.map(|commit| commit.receipt)
    }

    #[must_use]
    pub fn metrics(&self) -> CommandMetrics {
        let history = self.lock();
        CommandMetrics {
            completed: self.completed.load(std::sync::atomic::Ordering::Relaxed),
            p50_ms: percentile(&history.latencies, 50),
            p99_ms: percentile(&history.latencies, 99),
        }
    }

    /// A panic cannot corrupt these bounded queues, so recover their poisoned guard.
    fn lock(&self) -> std::sync::MutexGuard<'_, History> {
        self.history.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Claims and inserts under one lock so concurrent requests cannot submit the same key twice.
    fn claim(&self, key: &str, command: &ControlCommand) -> Claim {
        let mut history = self.lock();
        if let Some(entry) = history.receipts.iter().find(|entry| entry.key == key) {
            if entry.command != *command {
                return Claim::Conflict;
            }
            return match &entry.state {
                KeyState::Done(receipt) => Claim::Replay(receipt.clone()),
                KeyState::Pending(receiver) => Claim::Wait(receiver.clone()),
            };
        }
        let (sender, receiver) = watch::channel(());
        history.receipts.push_back(KeyEntry {
            key: key.to_owned(),
            command: command.clone(),
            state: KeyState::Pending(receiver),
        });
        evict_committed(&mut history.receipts, self.retained);
        drop(history);
        Claim::Execute(sender)
    }

    /// Commits receipts, reopens failed keys, then wakes waiters after releasing the lock so they observe
    /// the settled slot.
    fn settle(&self, key: &str, result: &Result<CommandReceipt, ControlError>) {
        let mut history = self.lock();
        match result {
            Ok(receipt) => {
                if let Some(entry) = history.receipts.iter_mut().find(|entry| entry.key == key) {
                    entry.state = KeyState::Done(receipt.clone());
                }
            }
            Err(_) => history.receipts.retain(|entry| entry.key != key),
        }
    }

    fn abandon(&self, key: &str) {
        self.lock()
            .receipts
            .retain(|entry| entry.key != key || !matches!(entry.state, KeyState::Pending(_)));
    }

    fn record(&self, latency: i64) {
        self.completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        push_bounded(&mut self.lock().latencies, latency, self.retained);
    }
}

#[async_trait::async_trait]
impl ControlExecutor for ControlPlane {
    async fn execute(
        &self,
        actor: &str,
        key: Option<&str>,
        command: ControlCommand,
    ) -> Result<CommandReceipt, ControlError> {
        Self::execute(self, actor, key, command).await
    }

    fn metrics(&self) -> ControlMetrics {
        let metrics = Self::metrics(self);
        ControlMetrics {
            completed: metrics.completed,
            p50_ms: metrics.p50_ms,
            p99_ms: metrics.p99_ms,
        }
    }
}

fn push_bounded<T>(queue: &mut VecDeque<T>, item: T, cap: usize) {
    queue.push_back(item);
    while queue.len() > cap {
        queue.pop_front();
    }
}

/// Never evicts pending claims from their owners or waiters.
fn evict_committed(receipts: &mut VecDeque<KeyEntry>, cap: usize) {
    while receipts.len() > cap {
        let Some(oldest) = receipts
            .iter()
            .position(|entry| matches!(entry.state, KeyState::Done(_)))
        else {
            break;
        };
        receipts.remove(oldest);
    }
}

/// The nearest-rank `pct`th percentile of `samples`, or zero when the window is empty.
fn percentile(samples: &VecDeque<i64>, pct: usize) -> i64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted: Vec<i64> = samples.iter().copied().collect();
    sorted.sort_unstable();
    let rank = pct * sorted.len();
    let index = (rank.div_ceil(100)).clamp(1, sorted.len()) - 1;
    sorted[index]
}

#[cfg(test)]
#[path = "../tests/unit/control_tests.rs"]
mod tests;
