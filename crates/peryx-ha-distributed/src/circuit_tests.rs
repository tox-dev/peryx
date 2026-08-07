use std::time::Duration;

use crate::circuit::{CircuitBreaker, CircuitConfig, DEFAULT_CIRCUIT};

fn breaker(trip_after: u32, cooldown_secs: u64) -> CircuitBreaker {
    CircuitBreaker::new(CircuitConfig {
        trip_after,
        cooldown: Duration::from_secs(cooldown_secs),
    })
}

#[test]
fn test_default_config_matches_the_documented_constant() {
    assert_eq!(CircuitConfig::default(), DEFAULT_CIRCUIT);
}

#[test]
fn test_an_unseen_source_is_available() {
    let breaker = breaker(3, 30);
    assert!(breaker.available("dc-a", Duration::ZERO));
}

#[test]
fn test_a_source_below_the_threshold_stays_available() {
    let mut breaker = breaker(3, 30);
    breaker.record_failure("dc-a", Duration::ZERO);
    breaker.record_failure("dc-a", Duration::ZERO);
    assert!(
        breaker.available("dc-a", Duration::ZERO),
        "two losses under a threshold of three"
    );
}

#[test]
fn test_reaching_the_threshold_trips_the_source_open() {
    let mut breaker = breaker(3, 30);
    for _ in 0..3 {
        breaker.record_failure("dc-a", Duration::from_secs(1));
    }
    assert!(
        !breaker.available("dc-a", Duration::from_secs(1)),
        "tripped during the cooldown"
    );
    assert!(
        !breaker.available("dc-a", Duration::from_secs(30)),
        "still open before the cooldown elapses"
    );
}

#[test]
fn test_a_tripped_source_admits_a_probe_after_the_cooldown() {
    let mut breaker = breaker(3, 30);
    for _ in 0..3 {
        breaker.record_failure("dc-a", Duration::from_secs(1));
    }
    assert!(
        breaker.available("dc-a", Duration::from_secs(31)),
        "the cooldown elapsed, so a probe may try"
    );
}

#[test]
fn test_a_successful_probe_closes_the_source() {
    let mut breaker = breaker(3, 30);
    for _ in 0..3 {
        breaker.record_failure("dc-a", Duration::from_secs(1));
    }
    breaker.record_success("dc-a");
    assert!(
        breaker.available("dc-a", Duration::ZERO),
        "a success closes the source at once"
    );
}

#[test]
fn test_a_failed_probe_reopens_the_source_for_a_fresh_cooldown() {
    let mut breaker = breaker(3, 30);
    for _ in 0..3 {
        breaker.record_failure("dc-a", Duration::from_secs(1));
    }
    breaker.record_failure("dc-a", Duration::from_secs(31));
    assert!(
        !breaker.available("dc-a", Duration::from_secs(31)),
        "the failed probe re-opens it"
    );
    assert!(
        breaker.available("dc-a", Duration::from_secs(61)),
        "for a fresh cooldown from the probe"
    );
}

#[test]
fn test_a_success_resets_the_failure_count_before_a_trip() {
    let mut breaker = breaker(3, 30);
    breaker.record_failure("dc-a", Duration::ZERO);
    breaker.record_failure("dc-a", Duration::ZERO);
    breaker.record_success("dc-a");
    breaker.record_failure("dc-a", Duration::ZERO);
    breaker.record_failure("dc-a", Duration::ZERO);
    assert!(
        breaker.available("dc-a", Duration::ZERO),
        "the reset means two fresh losses do not trip"
    );
}

#[test]
fn test_a_zero_threshold_trips_on_the_first_failure() {
    let mut breaker = breaker(0, 30);
    breaker.record_failure("dc-a", Duration::from_secs(1));
    assert!(
        !breaker.available("dc-a", Duration::from_secs(1)),
        "a zero threshold trips at once"
    );
}

#[test]
fn test_sources_trip_independently() {
    let mut breaker = breaker(1, 30);
    breaker.record_failure("dc-a", Duration::from_secs(1));
    assert!(!breaker.available("dc-a", Duration::from_secs(1)));
    assert!(
        breaker.available("dc-b", Duration::from_secs(1)),
        "one tripped source leaves the others alone"
    );
}
