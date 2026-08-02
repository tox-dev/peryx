use crate::consensus::{
    AppendOutcome, DEFAULT_LOG_LIMITS, LogEntry, LogIndex, MemoryRaftLog, RaftLog, RaftLogError, Term,
};
use crate::election::{NodeId, PersistentState, VoteRequest};
use crate::follower::{AppendReject, AppendRequest, CommitTracker, receive_append_entries};

fn entry(term: u64, index: u64, payload: &[u8]) -> LogEntry {
    LogEntry {
        term: Term(term),
        index: LogIndex(index),
        payload: payload.to_vec(),
    }
}

fn setup() -> (PersistentState, MemoryRaftLog, CommitTracker) {
    (
        PersistentState::new(),
        MemoryRaftLog::new(DEFAULT_LOG_LIMITS),
        CommitTracker::new(),
    )
}

fn request(
    leader_term: u64,
    prev_index: u64,
    prev_term: u64,
    entries: Vec<LogEntry>,
    leader_commit: u64,
) -> AppendRequest {
    AppendRequest {
        leader_term: Term(leader_term),
        prev_index: LogIndex(prev_index),
        prev_term: Term(prev_term),
        entries,
        leader_commit: LogIndex(leader_commit),
    }
}

#[test]
fn test_receive_appends_onto_an_empty_log_and_accepts() {
    let (mut state, mut log, mut commit) = setup();

    let response = receive_append_entries(
        &mut state,
        &mut log,
        &mut commit,
        &request(1, 0, 0, vec![entry(1, 1, b"a"), entry(1, 2, b"b")], 2),
    );

    assert!(response.accepted());
    assert_eq!(response.term, Term(1));
    let accepted = response.result.unwrap();
    assert_eq!(
        accepted.log,
        AppendOutcome {
            appended: 2,
            truncated: 0,
            last_index: LogIndex(2),
        }
    );
    assert_eq!(accepted.commit_index, LogIndex(2));
    assert!(accepted.committed);
    assert_eq!(state.current_term(), Term(1));
    assert_eq!(log.last_index(), LogIndex(2));
}

#[test]
fn test_receive_bumps_the_term_and_forgets_a_stale_vote() {
    let (mut state, mut log, mut commit) = setup();
    let granted = state.request_vote(
        &VoteRequest {
            candidate: NodeId(9),
            term: Term(2),
            last_log_index: LogIndex(0),
            last_log_term: Term(0),
        },
        &log,
    );
    assert!(granted.granted());

    let response = receive_append_entries(
        &mut state,
        &mut log,
        &mut commit,
        &request(5, 0, 0, vec![entry(5, 1, b"a")], 0),
    );

    assert!(response.accepted());
    assert_eq!(response.term, Term(5));
    assert_eq!(state.current_term(), Term(5));
    assert_eq!(state.voted_for(), None);
}

#[test]
fn test_receive_at_an_equal_term_keeps_the_vote() {
    let (mut state, mut log, mut commit) = setup();
    let granted = state.request_vote(
        &VoteRequest {
            candidate: NodeId(7),
            term: Term(3),
            last_log_index: LogIndex(0),
            last_log_term: Term(0),
        },
        &log,
    );
    assert!(granted.granted());

    let response = receive_append_entries(
        &mut state,
        &mut log,
        &mut commit,
        &request(3, 0, 0, vec![entry(3, 1, b"a")], 0),
    );

    assert!(response.accepted());
    assert_eq!(state.current_term(), Term(3));
    assert_eq!(state.voted_for(), Some(NodeId(7)));
}

#[test]
fn test_receive_rejects_a_stale_leader_term() {
    let (mut state, mut log, mut commit) = setup();
    state.observe_term(Term(5)).unwrap();

    let response = receive_append_entries(
        &mut state,
        &mut log,
        &mut commit,
        &request(3, 0, 0, vec![entry(3, 1, b"a")], 1),
    );

    assert!(!response.accepted());
    assert_eq!(response.term, Term(5));
    assert_eq!(response.result.unwrap_err(), AppendReject::StaleTerm);
    assert_eq!(state.current_term(), Term(5));
    assert_eq!(log.last_index(), LogIndex::ZERO);
    assert_eq!(commit.commit_index(), LogIndex::ZERO);
}

#[test]
fn test_receive_rejects_a_prev_log_mismatch() {
    let (mut state, mut log, mut commit) = setup();

    let response = receive_append_entries(
        &mut state,
        &mut log,
        &mut commit,
        &request(2, 3, 1, vec![entry(2, 4, b"z")], 0),
    );

    assert!(!response.accepted());
    assert!(matches!(
        response.result.unwrap_err(),
        AppendReject::Log(RaftLogError::MissingPrev {
            prev_index: 3,
            last_index: 0
        })
    ));
    assert_eq!(log.last_index(), LogIndex::ZERO);
}

#[test]
fn test_receive_truncates_a_conflicting_suffix() {
    let (mut state, mut log, mut commit) = setup();
    receive_append_entries(
        &mut state,
        &mut log,
        &mut commit,
        &request(
            2,
            0,
            0,
            vec![entry(1, 1, b"a"), entry(1, 2, b"b"), entry(2, 3, b"c")],
            0,
        ),
    );

    let response = receive_append_entries(
        &mut state,
        &mut log,
        &mut commit,
        &request(3, 1, 1, vec![entry(3, 2, b"x")], 0),
    );

    let accepted = response.result.unwrap();
    assert_eq!(
        accepted.log,
        AppendOutcome {
            appended: 1,
            truncated: 2,
            last_index: LogIndex(2),
        }
    );
    assert_eq!(log.entries(2..3), vec![entry(3, 2, b"x")]);
    assert_eq!(log.last_index(), LogIndex(2));
}

#[test]
fn test_receive_clamps_commit_to_the_last_new_entry() {
    let (mut state, mut log, mut commit) = setup();

    let response = receive_append_entries(
        &mut state,
        &mut log,
        &mut commit,
        &request(1, 0, 0, vec![entry(1, 1, b"a"), entry(1, 2, b"b")], 9),
    );

    let accepted = response.result.unwrap();
    assert!(accepted.committed);
    assert_eq!(accepted.commit_index, LogIndex(2));
    assert_eq!(commit.commit_index(), LogIndex(2));
}

#[test]
fn test_receive_heartbeat_does_not_lower_the_commit() {
    let (mut state, mut log, mut commit) = setup();
    receive_append_entries(
        &mut state,
        &mut log,
        &mut commit,
        &request(
            1,
            0,
            0,
            vec![entry(1, 1, b"a"), entry(1, 2, b"b"), entry(1, 3, b"c")],
            3,
        ),
    );

    let response = receive_append_entries(&mut state, &mut log, &mut commit, &request(1, 3, 1, vec![], 1));

    let accepted = response.result.unwrap();
    assert!(!accepted.committed);
    assert_eq!(accepted.commit_index, LogIndex(3));
    assert_eq!(commit.commit_index(), LogIndex(3));
}

#[test]
fn test_commit_tracker_starts_at_the_sentinel() {
    let tracker = CommitTracker::new();

    assert_eq!(tracker.commit_index(), LogIndex::ZERO);
    assert_eq!(tracker.last_applied(), LogIndex::ZERO);
    assert_eq!(CommitTracker::default(), tracker);
}

#[test]
fn test_follow_clamps_to_the_last_new_index() {
    let mut tracker = CommitTracker::new();

    let moved = tracker.follow(LogIndex(9), LogIndex(4));

    assert!(moved);
    assert_eq!(tracker.commit_index(), LogIndex(4));
}

#[test]
fn test_follow_clamps_to_the_leader_commit() {
    let mut tracker = CommitTracker::new();

    let moved = tracker.follow(LogIndex(4), LogIndex(9));

    assert!(moved);
    assert_eq!(tracker.commit_index(), LogIndex(4));
}

#[test]
fn test_follow_refuses_to_move_backward() {
    let mut tracker = CommitTracker::new();
    tracker.follow(LogIndex(5), LogIndex(5));

    let moved = tracker.follow(LogIndex(3), LogIndex(9));

    assert!(!moved);
    assert_eq!(tracker.commit_index(), LogIndex(5));
}

#[test]
fn test_follow_ignores_a_repeat_at_the_same_index() {
    let mut tracker = CommitTracker::new();
    tracker.follow(LogIndex(5), LogIndex(9));

    let moved = tracker.follow(LogIndex(5), LogIndex(9));

    assert!(!moved);
    assert_eq!(tracker.commit_index(), LogIndex(5));
}

#[test]
fn test_apply_next_walks_up_to_the_commit_index_then_stops() {
    let mut tracker = CommitTracker::new();
    tracker.follow(LogIndex(2), LogIndex(2));

    assert_eq!(tracker.apply_next(), Some(LogIndex(1)));
    assert_eq!(tracker.apply_next(), Some(LogIndex(2)));
    assert_eq!(tracker.apply_next(), None);
    assert_eq!(tracker.last_applied(), LogIndex(2));
}
