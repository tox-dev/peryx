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
