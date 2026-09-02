use std::collections::HashSet;

use super::*;
use crate::{AuthorityEpoch, OperationKind};

fn observation() -> OperationObservation {
    OperationObservation {
        source: "primary-a".to_owned(),
        authority: "repository:alpha".to_owned(),
        epoch: AuthorityEpoch(3),
        serial: Some(7),
        kind: OperationKind::Publish,
    }
}

#[test]
fn test_open_keeps_the_operation_identity() {
    let trace = OperationTrace::open(observation());

    assert_eq!(trace.operation, observation());
}

#[test]
fn test_open_builds_a_sampled_w3c_traceparent() {
    let traceparent = OperationTrace::open(observation()).traceparent;
    let fields = traceparent.split('-').collect::<Vec<_>>();

    assert_eq!(fields.len(), 4, "{traceparent}");
    assert_eq!(fields[0], "00");
    assert_eq!(fields[1].len(), 32);
    assert_eq!(fields[2].len(), 16);
    assert_eq!(fields[3], "01");
    assert!(
        traceparent.bytes().all(|byte| byte == b'-' || byte.is_ascii_hexdigit()),
        "{traceparent}"
    );
}

/// The trace-context specification rejects an all-zero trace or span identifier, and a receiver that
/// enforces it would drop the write's whole trace.
#[test]
fn test_open_never_builds_a_zero_identifier() {
    let traceparent = OperationTrace::open(observation()).traceparent;
    let fields = traceparent.split('-').collect::<Vec<_>>();

    assert_ne!(fields[1], "0".repeat(32));
    assert_ne!(fields[2], "0".repeat(16));
}

/// A hash of the operation's identity repeats whenever that identity does, which a replay under a new
/// epoch makes routine. Two traces opened for the same operation must still be two traces.
#[test]
fn test_open_draws_a_fresh_identifier_for_the_same_operation() {
    let traces = (0..64)
        .map(|_| OperationTrace::open(observation()).traceparent)
        .collect::<HashSet<_>>();

    assert_eq!(traces.len(), 64);
}
