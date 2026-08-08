use bytes::Bytes;

use super::{Inflight, ServingCache, flight_gate, release_flight, within_stale_bound};

#[tokio::test]
async fn test_same_key_waiters_share_one_gate() {
    let inflight = Inflight::default();
    let first = flight_gate(&inflight, "digest").lock_owned().await;
    assert!(flight_gate(&inflight, "digest").try_lock_owned().is_err());

    drop(first);
    drop(flight_gate(&inflight, "digest").try_lock_owned().unwrap());
}

#[tokio::test]
async fn test_distinct_keys_lock_independently() {
    let inflight = Inflight::default();
    let first = flight_gate(&inflight, "first").lock().await;
    let second = flight_gate(&inflight, "second").try_lock_owned().unwrap();

    drop((first, second));
}

#[tokio::test]
async fn test_cancelled_waiter_retires_its_registration() {
    let inflight = Inflight::default();
    let producer = flight_gate(&inflight, "digest").lock_owned().await;
    let mut waiting = tokio::spawn(flight_gate(&inflight, "digest").lock_owned());
    tokio::task::yield_now().await;

    waiting.abort();
    let cancelled = (&mut waiting).await.unwrap_err().is_cancelled();
    drop(waiting);
    assert!(cancelled);

    drop(producer);
    drop(flight_gate(&inflight, "digest").try_lock_owned().unwrap());
}

#[test]
fn test_active_counts_registered_callers() {
    let inflight = Inflight::default();
    assert_eq!(inflight.active("digest"), 0, "no gate exists for an unseen key");
    let first = flight_gate(&inflight, "digest");
    assert_eq!(inflight.active("digest"), 1);
    let second = flight_gate(&inflight, "digest");
    assert_eq!(inflight.active("digest"), 2, "a second caller shares the one gate");
    drop(second);
    assert_eq!(inflight.active("digest"), 1);
    drop(first);
    assert_eq!(
        inflight.active("digest"),
        0,
        "the gate retires when its last caller leaves"
    );
}

#[test]
fn test_release_flight_retires_the_gate() {
    let inflight = Inflight::default();

    release_flight(
        &inflight,
        "digest",
        flight_gate(&inflight, "digest").try_lock_owned().unwrap(),
    );

    assert_eq!(inflight.active("digest"), 0);
}

#[test]
fn test_forget_flight_retires_an_uncontended_gate() {
    let cache = ServingCache::new(1024, 60);
    let guard = flight_gate(&cache.inflight, "digest").try_lock_owned().unwrap();

    cache.forget_flight("digest");

    assert_eq!(cache.inflight.active("digest"), 0);
    drop(guard);
}

#[test]
fn test_hot_cache_honors_entry_expiry() {
    let cache = ServingCache::new(1024, 0);
    cache.store_hot("page".to_owned(), Bytes::from_static(b"body"), 10);

    assert_eq!(cache.hot_fresh("page", 9), Some(Bytes::from_static(b"body")));
    assert_eq!(cache.hot_fresh("page", 10), None);
    assert_eq!(cache.hot_fresh("missing", 0), None);
}

#[test]
fn test_versioned_hot_cache_returns_source_revision() {
    let cache = ServingCache::new(1024, 60);
    cache.store_hot_versioned("page".to_owned(), Bytes::from_static(b"body"), 10, Some(7));

    assert_eq!(
        cache.hot_fresh_versioned("page", 9),
        Some((Bytes::from_static(b"body"), Some(7)))
    );
    assert_eq!(cache.hot_fresh_versioned("page", 10), None);
    assert_eq!(cache.hot_fresh_versioned("missing", 0), None);
}

#[test]
fn test_hot_keys_change_only_for_the_invalidated_project() {
    let cache = ServingCache::new(1024, 60);
    let first = cache.hot_key("route", "first", "json");
    let second = cache.hot_key("route", "second", "json");

    cache.invalidate_hot("first");

    assert_ne!(cache.hot_key("route", "first", "json"), first);
    assert_eq!(cache.hot_key("route", "second", "json"), second);
}

#[test]
fn test_negative_cache_retires_expired_entries() {
    let cache = ServingCache::new(1024, 60);
    assert!(!cache.negative_fresh("missing", 0));

    cache.remember_negative("missing".to_owned(), 10);

    assert!(cache.negative_fresh("missing", 9));
    assert!(!cache.negative_fresh("missing", 10));
    assert!(!cache.negative_fresh("missing", 9));
}

#[test]
fn test_zero_max_stale_serves_any_age() {
    assert!(within_stale_bound(1_000_000, 0, 0, 60));
}

#[test]
fn test_stale_within_the_bound_serves_and_past_it_does_not() {
    // fresh for 60s, tolerate 300s past that: servable up to 360s after fetch.
    assert!(within_stale_bound(1_359, 300, 1_000, 60));
    assert!(!within_stale_bound(1_360, 300, 1_000, 60));
}

#[test]
fn test_a_future_fetch_time_does_not_underflow() {
    assert!(within_stale_bound(1_000, 300, 5_000, 60));
}

#[test]
fn test_a_stale_window_at_the_i64_ceiling_does_not_overflow() {
    // freshness_secs + max_stale_secs exceeds i64::MAX; the bound saturates rather than
    // overflowing (a panic in debug builds, a wrap to a negative window in release).
    assert!(within_stale_bound(1_000, 1, 0, i64::MAX));
}
