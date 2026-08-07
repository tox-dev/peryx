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
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Notify;

    use peryx_ha::{CommandOutcome, plan_voter_roster};

    use super::{
        AuditRecord, CommandReceipt, ControlCommand, ControlError, ControlPlane, KeyEntry, KeyState, MembershipControl,
        evict_committed, percentile,
    };
    use crate::state::Clock;
    use std::collections::{BTreeSet, VecDeque};
    use tokio::sync::watch;

    fn receipt(index: u64) -> CommandReceipt {
        CommandReceipt {
            term: 1,
            index,
            outcome: CommandOutcome::Committed,
            old_voters: Vec::new(),
            new_voters: Vec::new(),
        }
    }

    /// A control double returning a scripted result, counting its submissions so a test can prove a
    /// replay never reached it.
    struct ScriptedControl {
        results: Mutex<VecDeque<Result<CommandReceipt, ControlError>>>,
        submissions: Mutex<Vec<ControlCommand>>,
    }

    impl ScriptedControl {
        fn new(results: impl IntoIterator<Item = Result<CommandReceipt, ControlError>>) -> Arc<Self> {
            Arc::new(Self {
                results: Mutex::new(results.into_iter().collect()),
                submissions: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait::async_trait]
    impl MembershipControl for ScriptedControl {
        async fn submit(&self, command: ControlCommand) -> Result<CommandReceipt, ControlError> {
            self.submissions.lock().unwrap().push(command);
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .expect("the scripted control ran out of results")
        }
    }

    /// A control double that blocks inside `submit` until released, counting its submissions so a test can
    /// hold a command in flight and prove how many requests reached it.
    struct GatedControl {
        entered: Arc<Notify>,
        release: Arc<Notify>,
        submissions: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl MembershipControl for GatedControl {
        async fn submit(&self, _command: ControlCommand) -> Result<CommandReceipt, ControlError> {
            self.submissions.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            self.release.notified().await;
            Ok(receipt(1))
        }
    }

    fn fixed_clock() -> Clock {
        Arc::new(|| 0)
    }

    /// A clock that returns each scripted reading in turn, holding the last, so a command's start and end
    /// timestamps are deterministic.
    fn scripted_clock(readings: impl IntoIterator<Item = i64>) -> Clock {
        let readings = Mutex::new(readings.into_iter().collect::<VecDeque<_>>());
        Arc::new(move || {
            let mut readings = readings.lock().unwrap();
            let next = readings.pop_front().expect("the scripted clock ran out of readings");
            if readings.is_empty() {
                readings.push_back(next);
            }
            next
        })
    }

    fn transfer() -> ControlCommand {
        ControlCommand::TransferAuthority {
            authority: "proj".to_owned(),
            new_home: "west".to_owned(),
        }
    }

    #[tokio::test]
    async fn test_execute_returns_the_committed_receipt() {
        let control = ScriptedControl::new([Ok(receipt(7))]);
        let plane = ControlPlane::new(control.clone(), fixed_clock());

        let committed = plane.execute("alice", Some("k1"), transfer()).await.unwrap();

        assert_eq!(committed, receipt(7));
        assert_eq!(control.submissions.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_a_repeated_key_returns_one_committed_result_without_resubmitting() {
        // The second submission would fail; a replay must return the first receipt instead of reaching it.
        let control = ScriptedControl::new([Ok(receipt(7)), Err(ControlError::Unavailable("gone".to_owned()))]);
        let plane = ControlPlane::new(control.clone(), fixed_clock());

        let first = plane.execute("alice", Some("k1"), transfer()).await.unwrap();
        let replay = plane.execute("alice", Some("k1"), transfer()).await.unwrap();

        assert_eq!(first, replay);
        assert_eq!(
            control.submissions.lock().unwrap().len(),
            1,
            "the replay never reached the control"
        );
    }

    #[tokio::test]
    async fn test_a_replay_survives_a_leader_loss_after_commit() {
        // The command commits, then the node loses leadership; a retry on the same key reads the committed
        // receipt rather than the not-leader error a fresh submission would now hit.
        let control = ScriptedControl::new([Ok(receipt(3)), Err(ControlError::NotLeader { leader: None })]);
        let plane = ControlPlane::new(control, fixed_clock());

        let committed = plane.execute("alice", Some("k1"), transfer()).await.unwrap();
        let after_loss = plane.execute("alice", Some("k1"), transfer()).await.unwrap();

        assert_eq!(committed, after_loss);
    }

    #[tokio::test]
    async fn test_a_keyless_command_never_deduplicates() {
        let control = ScriptedControl::new([Ok(receipt(1)), Ok(receipt(2))]);
        let plane = ControlPlane::new(control.clone(), fixed_clock());

        plane.execute("alice", None, transfer()).await.unwrap();
        plane.execute("alice", None, transfer()).await.unwrap();

        assert_eq!(control.submissions.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_a_failure_is_returned_and_not_cached() {
        let control = ScriptedControl::new([Err(ControlError::Invalid("same home".to_owned())), Ok(receipt(5))]);
        let plane = ControlPlane::new(control, fixed_clock());

        let failed = plane.execute("alice", Some("k1"), transfer()).await;
        let retried = plane.execute("alice", Some("k1"), transfer()).await.unwrap();

        assert_eq!(failed, Err(ControlError::Invalid("same home".to_owned())));
        assert_eq!(retried, receipt(5), "a failure leaves the key open to a later commit");
    }

    #[rstest::rstest]
    #[case::not_leader(ControlError::NotLeader { leader: None })]
    #[case::unavailable(ControlError::Unavailable("gone".to_owned()))]
    #[case::invalid(ControlError::Invalid("same home".to_owned()))]
    #[tokio::test]
    async fn test_each_failure_kind_is_returned_and_audited(#[case] error: ControlError) {
        // Each failure runs the audit path, so its kind is named without leaking the request body.
        let control = ScriptedControl::new([Err(error.clone())]);
        let plane = ControlPlane::new(control, fixed_clock());

        assert_eq!(plane.execute("alice", None, transfer()).await, Err(error));
    }

    #[tokio::test]
    async fn test_concurrent_requests_on_one_key_reach_the_command_once() {
        // The owner holds the command in flight while a second request retries the same key. The retry must
        // wait on the claimed key and replay the committed receipt rather than reach a second submission.
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let submissions = Arc::new(AtomicUsize::new(0));
        let control = Arc::new(GatedControl {
            entered: entered.clone(),
            release: release.clone(),
            submissions: submissions.clone(),
        });
        let plane = Arc::new(ControlPlane::new(control, fixed_clock()));

        let owner = tokio::spawn({
            let plane = plane.clone();
            async move { plane.execute("alice", Some("k1"), transfer()).await }
        });
        entered.notified().await;

        let waiter = tokio::spawn({
            let plane = plane.clone();
            async move { plane.execute("bob", Some("k1"), transfer()).await }
        });
        tokio::task::yield_now().await;
        release.notify_one();

        assert_eq!(owner.await.unwrap().unwrap(), receipt(1));
        assert_eq!(
            waiter.await.unwrap().unwrap(),
            receipt(1),
            "the retry replayed the owner's receipt"
        );
        assert_eq!(
            submissions.load(Ordering::SeqCst),
            1,
            "the retry never reached a second submission"
        );
    }

    #[tokio::test]
    async fn test_a_key_reused_for_a_different_command_is_rejected() {
        let control = ScriptedControl::new([Ok(receipt(7))]);
        let plane = ControlPlane::new(control.clone(), fixed_clock());

        plane.execute("alice", Some("k1"), transfer()).await.unwrap();
        let other = ControlCommand::AdvanceEpoch {
            authority: "proj".to_owned(),
        };
        let reused = plane.execute("alice", Some("k1"), other).await;

        assert_eq!(reused, Err(ControlError::KeyReuse));
        assert_eq!(
            control.submissions.lock().unwrap().len(),
            1,
            "the reused key never reached a second command"
        );
    }

    #[tokio::test]
    async fn test_the_concurrency_bound_rejects_an_excess_command() {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let control = Arc::new(GatedControl {
            entered: entered.clone(),
            release: release.clone(),
            submissions: Arc::new(AtomicUsize::new(0)),
        });
        let plane = Arc::new(ControlPlane::with_limits(control, fixed_clock(), 1, 8));

        let held = plane.clone();
        let holding = tokio::spawn(async move { held.execute("alice", None, transfer()).await });
        entered.notified().await;

        let overflow = plane.execute("bob", None, transfer()).await;
        assert_eq!(overflow, Err(ControlError::Overloaded));

        release.notify_one();
        assert_eq!(holding.await.unwrap().unwrap(), receipt(1));
    }

    #[tokio::test]
    async fn test_metrics_report_the_completed_count_and_latency_percentiles() {
        let control = ScriptedControl::new([Ok(receipt(1)), Ok(receipt(2)), Ok(receipt(3)), Ok(receipt(4))]);
        // Each command reads the clock at start and end: latencies 1, 5, 10, 100 ms.
        let plane = ControlPlane::new(control, scripted_clock([0, 1, 0, 5, 0, 10, 0, 100]));

        for _ in 0..4 {
            plane.execute("alice", None, transfer()).await.unwrap();
        }

        let metrics = plane.metrics();
        assert_eq!(metrics.completed, 4);
        assert_eq!(metrics.p50_ms, 5);
        assert_eq!(metrics.p99_ms, 100);
    }

    #[tokio::test]
    async fn test_the_idempotency_window_evicts_the_oldest_receipt() {
        let results = (0..3).map(|index| Ok(receipt(index))).chain([Ok(receipt(99))]);
        let control = ScriptedControl::new(results);
        let plane = ControlPlane::with_limits(control.clone(), fixed_clock(), 4, 2);

        for key in ["k0", "k1", "k2"] {
            plane.execute("alice", Some(key), transfer()).await.unwrap();
        }
        // k0 fell out of the two-slot window, so its retry resubmits rather than replaying.
        plane.execute("alice", Some("k0"), transfer()).await.unwrap();

        assert_eq!(control.submissions.lock().unwrap().len(), 4);
    }

    #[test]
    fn test_percentile_of_an_empty_window_is_zero() {
        assert_eq!(percentile(&VecDeque::new(), 99), 0);
    }

    #[test]
    fn test_eviction_keeps_in_flight_slots_over_cap() {
        // Two commands are in flight with a one-slot cap: neither slot is committed, so eviction leaves
        // both rather than drop a pending claim from under the request that owns it.
        let pending = |key: &str| {
            let (_sender, receiver) = watch::channel(());
            KeyEntry {
                key: key.to_owned(),
                fingerprint: 0,
                state: KeyState::Pending(receiver),
            }
        };
        let mut receipts = VecDeque::from([pending("k0"), pending("k1")]);

        evict_committed(&mut receipts, 1);

        assert_eq!(receipts.len(), 2);
    }

    #[test]
    fn test_plan_voter_roster_adds_and_removes() {
        let current = BTreeSet::from([1, 2, 3]);

        assert_eq!(plan_voter_roster(&current, Some(4), None), BTreeSet::from([1, 2, 3, 4]));
        assert_eq!(plan_voter_roster(&current, None, Some(2)), BTreeSet::from([1, 3]));
        assert_eq!(plan_voter_roster(&current, Some(4), Some(1)), BTreeSet::from([2, 3, 4]));
    }

    #[test]
    fn test_plan_voter_roster_is_a_no_op_for_a_present_add_or_absent_remove() {
        let current = BTreeSet::from([1, 2]);

        assert_eq!(plan_voter_roster(&current, Some(1), Some(9)), current);
    }

    #[test]
    fn test_audit_records_name_the_command_without_the_body() {
        let committed = AuditRecord::committed("alice", &transfer(), &receipt(4));
        assert_eq!(committed.actor, "alice");
        assert_eq!(committed.command, "transfer_authority");
        assert_eq!(committed.target, "proj");
        assert_eq!(committed.result, "committed");
        assert_eq!(committed.term, Some(1));
        assert_eq!(committed.index, Some(4));

        let learner = ControlCommand::AddLearner {
            datacenter: "west".to_owned(),
            address: "west.internal:4460".to_owned(),
        };
        let failed = AuditRecord::failed("alice", &learner, &ControlError::NotLeader { leader: None });
        assert_eq!(failed.command, "add_learner");
        assert_eq!(failed.target, "west");
        assert_eq!(failed.result, "not_leader");
        assert_eq!(failed.term, None);
        let rendered = serde_json::to_string(&failed).unwrap();
        assert!(
            !rendered.contains("west.internal:4460"),
            "the audit record must not carry the request body: {rendered}"
        );
    }

    #[test]
    fn test_a_no_change_receipt_audits_as_no_change() {
        let no_change = CommandReceipt {
            term: 2,
            index: 8,
            outcome: CommandOutcome::NoChange,
            old_voters: Vec::new(),
            new_voters: Vec::new(),
        };
        let record = AuditRecord::committed("alice", &transfer(), &no_change);
        assert_eq!(record.result, "no_change");
    }

    #[test]
    fn test_a_committed_membership_receipt_audits_the_old_and_new_voter_sets() {
        let promote = ControlCommand::PromoteVoter {
            datacenter: "west".to_owned(),
        };
        let receipt = CommandReceipt {
            term: 3,
            index: 12,
            outcome: CommandOutcome::Committed,
            old_voters: vec!["east".to_owned()],
            new_voters: vec!["east".to_owned(), "west".to_owned()],
        };
        let record = AuditRecord::committed("alice", &promote, &receipt);
        assert_eq!(record.old_voters, ["east"]);
        assert_eq!(record.new_voters, ["east", "west"]);

        // A failed command carries no roster transition.
        let failed = AuditRecord::failed("alice", &promote, &ControlError::NotLeader { leader: None });
        assert!(failed.old_voters.is_empty() && failed.new_voters.is_empty());
    }

    #[test]
    fn test_a_replay_audit_names_the_replay_result() {
        let record = AuditRecord::replayed("alice", &transfer(), &receipt(4));
        assert_eq!(record.result, "replayed");
        assert_eq!(record.index, Some(4));
    }

    #[test]
    fn test_command_kind_and_target_cover_every_variant() {
        for (command, kind, target) in [
            (
                ControlCommand::PromoteVoter {
                    datacenter: "west".to_owned(),
                },
                "promote_voter",
                "west",
            ),
            (
                ControlCommand::RemoveVoter {
                    datacenter: "west".to_owned(),
                },
                "remove_voter",
                "west",
            ),
            (
                ControlCommand::ReplaceVoter {
                    remove: "east".to_owned(),
                    datacenter: "west".to_owned(),
                    address: "west.internal:4460".to_owned(),
                },
                "replace_voter",
                "west",
            ),
            (
                ControlCommand::AdvanceEpoch {
                    authority: "proj".to_owned(),
                },
                "advance_epoch",
                "proj",
            ),
        ] {
            assert_eq!(command.kind(), kind);
            assert_eq!(command.target(), target);
        }
    }

    #[test]
    fn test_control_error_messages_name_the_cause() {
        assert_eq!(
            ControlError::NotLeader {
                leader: Some("east.internal:4460".to_owned()),
            }
            .to_string(),
            "not the consensus leader; leader at east.internal:4460",
        );
        assert_eq!(
            ControlError::NotLeader { leader: None }.to_string(),
            "not the consensus leader"
        );
        assert_eq!(
            ControlError::Unavailable("log gone".to_owned()).to_string(),
            "consensus command did not commit: log gone",
        );
        assert_eq!(
            ControlError::Invalid("same home".to_owned()).to_string(),
            "invalid command: same home",
        );
        assert_eq!(
            ControlError::Overloaded.to_string(),
            "too many concurrent availability commands in flight",
        );
        assert_eq!(
            ControlError::KeyReuse.to_string(),
            "idempotency key already used for a different command",
        );
    }

    #[test]
    fn test_the_outcome_serializes_to_its_snake_case_name() {
        assert_eq!(
            serde_json::to_string(&CommandOutcome::NoChange).unwrap(),
            "\"no_change\"",
        );
    }
}
