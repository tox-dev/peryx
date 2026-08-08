use std::num::NonZeroU32;
use std::time::Duration;

use crate::backoff::{DEFAULT_RECONNECT_POLICY, RETRY_EXHAUSTED, ReconnectPolicy, Retry};
use crate::peer::TransportError;

fn policy(base_ms: u64, multiplier: u32, cap_ms: u64, max_attempts: u32) -> ReconnectPolicy {
    ReconnectPolicy::new(
        Duration::from_millis(base_ms),
        NonZeroU32::new(multiplier).unwrap(),
        Duration::from_millis(cap_ms),
        NonZeroU32::new(max_attempts).unwrap(),
    )
}

#[test]
fn test_backoff_grows_each_attempt_then_holds_at_the_cap() {
    let policy = policy(100, 2, 1000, 10);
    let observed: Vec<Duration> = (1..=6).map(|attempt| policy.delay_for(attempt)).collect();

    assert_eq!(
        observed,
        vec![
            Duration::from_millis(100),
            Duration::from_millis(200),
            Duration::from_millis(400),
            Duration::from_millis(800),
            Duration::from_secs(1),
            Duration::from_secs(1),
        ]
    );
}

#[test]
fn test_delay_saturates_to_the_cap_on_multiply_overflow() {
    let ceiling = Duration::from_secs(u64::MAX);
    let policy = ReconnectPolicy::new(
        ceiling,
        NonZeroU32::new(2).unwrap(),
        ceiling,
        NonZeroU32::new(3).unwrap(),
    );

    assert_eq!(policy.delay_for(2), ceiling);
}

#[test]
fn test_base_above_the_cap_yields_the_cap() {
    let policy = policy(5000, 2, 1000, 3);

    assert_eq!(policy.delay_for(1), Duration::from_secs(1));
}

#[test]
fn test_retryable_error_within_budget_backs_off_by_the_delay() {
    let policy = policy(100, 2, 1000, 10);

    assert_eq!(
        policy.on_error(&TransportError::Disconnected, 1),
        Retry::After(Duration::from_millis(100))
    );
    assert_eq!(
        policy.on_error(&TransportError::Timeout, 3),
        Retry::After(Duration::from_millis(400))
    );
}

#[test]
fn test_retryable_error_retries_below_the_ceiling_and_gives_up_at_it() {
    let policy = policy(100, 2, 1000, 10);

    assert_eq!(
        policy.on_error(&TransportError::Timeout, 9),
        Retry::After(Duration::from_secs(1))
    );
    assert_eq!(
        policy.on_error(&TransportError::Timeout, 10),
        Retry::GiveUp {
            reason: RETRY_EXHAUSTED
        }
    );
    assert_eq!(
        policy.on_error(&TransportError::Disconnected, 11),
        Retry::GiveUp {
            reason: RETRY_EXHAUSTED
        }
    );
}

#[test]
fn test_terminal_error_gives_up_with_its_own_reason_regardless_of_attempt() {
    let policy = policy(100, 2, 1000, 10);
    let cases = [
        (TransportError::Unauthenticated, "unauthenticated"),
        (TransportError::FrameTooLarge { limit: 8, actual: 9 }, "frame_too_large"),
        (
            TransportError::TooManyOperations { limit: 8, actual: 9 },
            "too_many_operations",
        ),
        (
            TransportError::SourceChanged {
                expected: "a".to_owned(),
                actual: "b".to_owned(),
            },
            "source_changed",
        ),
        (TransportError::FrontierGap { expected: 4, actual: 6 }, "frontier_gap"),
        (TransportError::EmptyBatch { frontier: 5, after: 2 }, "empty_batch"),
    ];

    for (error, reason) in cases {
        assert_eq!(policy.on_error(&error, 1), Retry::GiveUp { reason });
    }
}

#[test]
fn test_default_matches_the_named_constant() {
    assert_eq!(ReconnectPolicy::default(), DEFAULT_RECONNECT_POLICY);
}
