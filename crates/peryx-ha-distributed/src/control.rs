use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use peryx_ha::{CommandReceipt, ControlCommand, ControlError, ControlExecutor, ControlMetrics, MembershipControl};
use serde::Serialize;
use tokio::sync::{Semaphore, watch};

use peryx_core::Clock;

const MAX_CONCURRENT_COMMANDS: usize = 4;

const RETAINED: usize = 256;

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

    fn emit(&self) {
        tracing::info!(
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

struct KeyEntry {
    key: String,
    fingerprint: u64,
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

#[derive(Default)]
struct History {
    receipts: VecDeque<KeyEntry>,
    latencies: VecDeque<i64>,
}

pub struct ControlPlane {
    control: Arc<dyn MembershipControl>,
    clock: Clock,
    permits: Semaphore,
    retained: usize,
    completed: std::sync::atomic::AtomicU64,
    history: Mutex<History>,
}

impl ControlPlane {
    #[must_use]
    pub fn new(control: Arc<dyn MembershipControl>, clock: Clock) -> Self {
        Self::with_limits(control, clock, MAX_CONCURRENT_COMMANDS, RETAINED)
    }

    fn with_limits(control: Arc<dyn MembershipControl>, clock: Clock, concurrency: usize, retained: usize) -> Self {
        Self {
            control,
            clock,
            permits: Semaphore::new(concurrency),
            retained,
            completed: std::sync::atomic::AtomicU64::new(0),
            history: Mutex::new(History::default()),
        }
    }

    /// Binds `key` to the command fingerprint before submission. Concurrent retries wait and replay the
    /// committed receipt; reuse with a different command fails.
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
            return self.run(actor, &command).await;
        };
        let fingerprint = fingerprint(&command);
        loop {
            match self.claim(key, fingerprint) {
                Claim::Replay(receipt) => {
                    AuditRecord::replayed(actor, &command, &receipt).emit();
                    return Ok(receipt);
                }
                Claim::Conflict => {
                    let error = ControlError::KeyReuse;
                    AuditRecord::failed(actor, &command, &error).emit();
                    return Err(error);
                }
                Claim::Execute(sender) => {
                    let result = self.run(actor, &command).await;
                    self.settle(key, &result, sender);
                    return result;
                }
                Claim::Wait(mut receiver) => {
                    let _ = receiver.changed().await;
                }
            }
        }
    }

    async fn run(&self, actor: &str, command: &ControlCommand) -> Result<CommandReceipt, ControlError> {
        let Ok(_permit) = self.permits.try_acquire() else {
            let error = ControlError::Overloaded;
            AuditRecord::failed(actor, command, &error).emit();
            return Err(error);
        };
        let started = (self.clock)();
        let result = self.control.submit(command.clone()).await;
        self.record(((self.clock)() - started).max(0));
        match &result {
            Ok(receipt) => AuditRecord::committed(actor, command, receipt).emit(),
            Err(error) => AuditRecord::failed(actor, command, error).emit(),
        }
        result
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
    fn claim(&self, key: &str, fingerprint: u64) -> Claim {
        let mut history = self.lock();
        if let Some(entry) = history.receipts.iter().find(|entry| entry.key == key) {
            if entry.fingerprint != fingerprint {
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
            fingerprint,
            state: KeyState::Pending(receiver),
        });
        evict_committed(&mut history.receipts, self.retained);
        drop(history);
        Claim::Execute(sender)
    }

    /// Commits receipts, reopens failed keys, then wakes waiters after releasing the lock so they observe
    /// the settled slot.
    fn settle(&self, key: &str, result: &Result<CommandReceipt, ControlError>, sender: watch::Sender<()>) {
        let mut history = self.lock();
        match result {
            Ok(receipt) => {
                if let Some(entry) = history.receipts.iter_mut().find(|entry| entry.key == key) {
                    entry.state = KeyState::Done(receipt.clone());
                }
            }
            Err(_) => history.receipts.retain(|entry| entry.key != key),
        }
        drop(history);
        drop(sender);
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

fn fingerprint(command: &ControlCommand) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    command.hash(&mut hasher);
    hasher.finish()
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
