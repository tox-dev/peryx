use std::num::NonZeroUsize;

use crate::failover::{Candidate, Failover, FailoverPolicy};
use crate::liveness::Suspicion;
use crate::ownership::DatacenterId;

fn candidate(datacenter: &str, suspicion: Suspicion) -> Candidate {
    Candidate {
        datacenter: DatacenterId(datacenter.to_owned()),
        suspicion,
    }
}

fn policy(max_candidates: usize) -> FailoverPolicy {
    FailoverPolicy::new(NonZeroUsize::new(max_candidates).expect("nonzero"))
}

#[test]
fn test_an_alive_home_holds_its_authority() {
    let decision = policy(8).select(Suspicion::Alive, &[candidate("west", Suspicion::Alive)]);
    assert_eq!(decision, Failover::Hold);
}

#[test]
fn test_a_suspect_home_holds_because_suspicion_never_moves_authority() {
    let decision = policy(8).select(Suspicion::Suspect, &[candidate("west", Suspicion::Alive)]);
    assert_eq!(decision, Failover::Hold);
}

#[test]
fn test_an_unheard_home_holds() {
    let decision = policy(8).select(Suspicion::Unknown, &[candidate("west", Suspicion::Alive)]);
    assert_eq!(decision, Failover::Hold);
}

#[test]
fn test_a_dead_home_transfers_to_the_alive_candidate() {
    let decision = policy(8).select(Suspicion::Dead, &[candidate("west", Suspicion::Alive)]);
    assert_eq!(decision, Failover::Transfer(DatacenterId("west".to_owned())));
}

#[test]
fn test_a_dead_home_takes_the_first_alive_candidate_in_order() {
    let candidates = [
        candidate("north", Suspicion::Suspect),
        candidate("west", Suspicion::Alive),
        candidate("south", Suspicion::Alive),
    ];
    let decision = policy(8).select(Suspicion::Dead, &candidates);
    assert_eq!(decision, Failover::Transfer(DatacenterId("west".to_owned())));
}

#[test]
fn test_a_dead_home_with_no_alive_candidate_finds_none() {
    let candidates = [
        candidate("north", Suspicion::Suspect),
        candidate("west", Suspicion::Dead),
        candidate("south", Suspicion::Unknown),
    ];
    let decision = policy(8).select(Suspicion::Dead, &candidates);
    assert_eq!(decision, Failover::NoCandidate);
}

#[test]
fn test_a_dead_home_with_an_empty_roster_finds_none() {
    let decision = policy(8).select(Suspicion::Dead, &[]);
    assert_eq!(decision, Failover::NoCandidate);
}

#[test]
fn test_evaluation_is_bounded_and_ignores_a_candidate_past_the_limit() {
    let candidates = [
        candidate("a", Suspicion::Dead),
        candidate("b", Suspicion::Dead),
        candidate("c", Suspicion::Alive),
    ];
    let decision = policy(2).select(Suspicion::Dead, &candidates);
    assert_eq!(decision, Failover::NoCandidate);
}

#[test]
fn test_the_bound_reaches_a_candidate_at_the_limit() {
    let candidates = [candidate("a", Suspicion::Dead), candidate("b", Suspicion::Alive)];
    let decision = policy(2).select(Suspicion::Dead, &candidates);
    assert_eq!(decision, Failover::Transfer(DatacenterId("b".to_owned())));
}
