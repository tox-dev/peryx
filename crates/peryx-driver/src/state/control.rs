//! Idempotency, admission, audit, and latency tracking for availability commands.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use peryx_ha::{CommandReceipt, ControlCommand, ControlError, MembershipControl};
use serde::Serialize;
use tokio::sync::{Semaphore, watch};

use crate::state::Clock;

const MAX_CONCURRENT_COMMANDS: usize = 4;

const RETAINED: usize = 256;

/// One audited control attempt.
///
/// Names the actor, the command's kind and target, the result, and the committed identity when it
/// committed. It never carries the request body, so an address or token is not logged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditRecord {
    /// The administrator that submitted the command.
    pub actor: String,
    /// The command's kind.
    pub command: &'static str,
    /// The command's datacenter or authority target.
    pub target: String,
    /// The attempt's result: a committed outcome, a replay, or a failure kind.
    pub result: &'static str,
    /// The committed term, present only when the command committed.
    pub term: Option<u64>,
    /// The committed index, present only when the command committed.
    pub index: Option<u64>,
    /// The voter roster before a committed membership command, so an auditor sees the roster transition.
    /// Empty for a failed command or one that does not touch the voter roster.
    pub old_voters: Vec<String>,
    /// The voter roster after a committed membership command.
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

/// The recent command latencies reported through the status resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CommandMetrics {
    /// The total commands the plane has completed since start, replays excluded.
    pub completed: u64,
    /// The median command latency over the retained window, in milliseconds.
    pub p50_ms: i64,
    /// The 99th-percentile command latency over the retained window, in milliseconds. Reported so a
    /// latency spike through a leader change is visible on the status resource.
    pub p99_ms: i64,
}

/// One idempotency slot in the bounded window: the key, a fingerprint of the command that claimed it, and
/// whether that command is still in flight or has committed.
struct KeyEntry {
    key: String,
    fingerprint: u64,
    state: KeyState,
}

/// The state of a claimed idempotency key.
enum KeyState {
    /// The command is in flight; a concurrent retry clones this receiver and wakes when the owner drops
    /// the paired sender on settle. A `watch` receiver is version-tracked, so a retry that clones it
    /// under the lock never misses the drop that the owner then performs.
    Pending(watch::Receiver<()>),
    /// The command committed; a retry on the key replays this receipt.
    Done(CommandReceipt),
}

/// The atomic outcome of claiming an idempotency key against the current window.
enum Claim {
    /// The caller owns the in-flight command and settles the slot when it resolves. Dropping the sender
    /// wakes every waiter, which then reclaims and reads the settled outcome.
    Execute(watch::Sender<()>),
    /// The key already committed this exact command; replay its receipt.
    Replay(CommandReceipt),
    /// Another request holds the key in flight; wait on its receiver, then reclaim.
    Wait(watch::Receiver<()>),
    /// The key is held under a different command fingerprint.
    Conflict,
}

/// The bounded recent history the plane keeps behind one lock: the idempotency slots and the latency
/// samples, each a recent-history window rather than a growing ledger.
#[derive(Default)]
struct History {
    receipts: VecDeque<KeyEntry>,
    latencies: VecDeque<i64>,
}

/// Wraps a [`MembershipControl`] with idempotency, a concurrency bound, audit, and latency reporting.
pub struct ControlPlane {
    control: Arc<dyn MembershipControl>,
    clock: Clock,
    permits: Semaphore,
    retained: usize,
    completed: std::sync::atomic::AtomicU64,
    history: Mutex<History>,
}

impl ControlPlane {
    /// Wrap `control`, reading time from `clock`, with the default concurrency and retention bounds.
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

    /// Run `command` for `actor`, deduplicating on `key` and auditing the attempt.
    ///
    /// A keyed request atomically claims its key before it executes, binding it to a fingerprint of the
    /// command body. A concurrent retry on the same key waits on the in-flight command and replays its
    /// committed receipt rather than reaching a second submission, so a client that retries after a leader
    /// loss reads one committed result. A key reused for a different command is rejected. Otherwise the
    /// command runs under a concurrency permit, its latency is recorded, its receipt is retained under the
    /// key, and one audit line names the actor, command, target, result, and committed identity.
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

    /// Submit `command` for `actor` under a concurrency permit, recording its latency and auditing the
    /// attempt. This is the non-idempotent core: a keyless command runs it directly, and a keyed command
    /// runs it once as the owner of its claimed key.
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

    /// The recent command latencies for the status resource.
    #[must_use]
    pub fn metrics(&self) -> CommandMetrics {
        let history = self.lock();
        CommandMetrics {
            completed: self.completed.load(std::sync::atomic::Ordering::Relaxed),
            p50_ms: percentile(&history.latencies, 50),
            p99_ms: percentile(&history.latencies, 99),
        }
    }

    /// Lock the history, recovering the guard rather than panicking if a prior holder poisoned it: the
    /// guarded data is bounded queues no panic can leave inconsistent.
    fn lock(&self) -> std::sync::MutexGuard<'_, History> {
        self.history.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Atomically claim `key` for a command with `fingerprint` under one lock: replay a committed receipt,
    /// wait on an in-flight command, reject a fingerprint mismatch, or insert a fresh pending slot and own
    /// the execution. Claiming and inserting in the same critical section is what stops two concurrent
    /// same-key requests from both reaching a submission.
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

    /// Resolve the owned key slot once its command settles, then wake every waiter so it reclaims and reads
    /// the outcome: on commit, replace the pending slot with the receipt so a retry replays it; on failure,
    /// drop the slot so the key reopens to a later attempt. Releasing the lock before dropping `sender`
    /// lets a woken waiter take the lock and observe the settled slot rather than the pending one.
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

fn fingerprint(command: &ControlCommand) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    command.hash(&mut hasher);
    hasher.finish()
}

/// Push `item` onto `queue`, evicting from the front until it holds at most `cap`, so a recent-history
/// window never grows past its bound.
fn push_bounded<T>(queue: &mut VecDeque<T>, item: T, cap: usize) {
    queue.push_back(item);
    while queue.len() > cap {
        queue.pop_front();
    }
}

/// Evict the oldest committed slots until the window holds at most `cap`, leaving in-flight slots in place
/// so a pending claim is never dropped from under the request that owns it or the waiters parked on it.
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
#[path = "../../tests/unit/state/control/tests.rs"]
mod tests;
