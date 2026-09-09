use std::future::Future;
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use rstest::rstest;

use super::{Inflight, ServingCache, flight_gate, negative_weight, release_flight, within_stale_bound};

/// Bounds the waits that fail by never resolving; the paused clock fires it as soon as nothing else can run.
const NEVER: std::time::Duration = std::time::Duration::from_mins(1);

#[tokio::test]
async fn test_same_key_waiters_share_one_gate() {
    let inflight = Inflight::default();
    let first = flight_gate(&inflight, "digest").lock_owned().await;
    assert!(flight_gate(&inflight, "digest").try_lock_owned().is_err());
    assert!(flight_gate(&inflight, "digest").try_lock_owned().is_err());

    drop(first);
    drop(flight_gate(&inflight, "digest").try_lock_owned().unwrap());
}

#[tokio::test]
async fn test_flight_subscription_reports_the_next_owner() {
    let inflight = Inflight::default();
    let first = flight_gate(&inflight, "digest");
    let mut events = inflight.subscribe("digest").unwrap();
    let mut next_join = std::pin::pin!(events.next_join());
    assert!(matches!(
        next_join.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));

    let second = flight_gate(&inflight, "digest");

    next_join.await.expect("the next owner joins the flight");
    drop((first, second));
}

#[tokio::test(start_paused = true)]
async fn test_flight_subscription_closes_with_the_flight() {
    let inflight = Inflight::default();
    let flight = flight_gate(&inflight, "digest");
    let mut events = inflight.subscribe("digest").unwrap();

    drop(flight);

    let closed = tokio::time::timeout(NEVER, events.next_join())
        .await
        .expect("the last owner leaving closes the flight");
    assert!(closed.is_err());
}

#[tokio::test]
async fn test_flight_stays_shared_while_an_owner_remains() {
    let inflight = Inflight::default();
    let first = flight_gate(&inflight, "digest");
    let second = flight_gate(&inflight, "digest");

    drop(first);

    let held = second.try_lock_owned().unwrap();
    assert!(flight_gate(&inflight, "digest").try_lock_owned().is_err());
    drop(held);
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
    {
        let mut waiting = std::pin::pin!(flight_gate(&inflight, "digest").lock_owned());
        assert!(matches!(
            waiting.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
    }
    assert!(flight_gate(&inflight, "digest").try_lock_owned().is_err());

    drop(producer);
    drop(flight_gate(&inflight, "digest").try_lock_owned().unwrap());
}

#[tokio::test]
async fn test_release_flight_retires_the_gate() {
    let inflight = Inflight::default();
    let flight = flight_gate(&inflight, "digest");

    release_flight(&inflight, "digest", flight.try_lock_owned().unwrap());

    drop(flight_gate(&inflight, "digest").try_lock_owned().unwrap());
}

#[test]
fn test_forget_flight_retires_an_uncontended_gate() {
    let cache = ServingCache::new(1024, 60);
    drop(flight_gate(&cache.inflight, "digest"));
    let stale = flight_gate(&cache.inflight, "digest").try_lock_owned().unwrap();

    cache.forget_flight("digest");

    let replacement = flight_gate(&cache.inflight, "digest").try_lock_owned().unwrap();
    drop(stale);
    assert!(flight_gate(&cache.inflight, "digest").try_lock_owned().is_err());
    drop(replacement);
    drop(flight_gate(&cache.inflight, "digest").try_lock_owned().unwrap());
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
fn test_representation_keys_change_only_for_the_invalidated_route_and_resource() {
    let cache = ServingCache::new(1024, 60);
    let first = cache.representation_key("route", "first", "json");
    let second = cache.representation_key("route", "second", "json");
    let independent = cache.representation_key("independent", "first", "json");

    cache.invalidate_resource("route", "first");

    assert_ne!(cache.representation_key("route", "first", "json"), first);
    assert_eq!(cache.representation_key("route", "second", "json"), second);
    assert_eq!(cache.representation_key("independent", "first", "json"), independent);
}

#[test]
fn test_concurrent_invalidations_advance_distinct_epochs() {
    let cache = ServingCache::new(1024, 60);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| cache.invalidate_resource("route", "resource"));
        }
    });

    assert_eq!(
        cache.representation_key("route", "resource", "json"),
        "route\0resource\0json\08"
    );
}

#[test]
fn test_negative_cache_retires_expired_entries() {
    let cache = ServingCache::new(1024, 60);
    assert!(!cache.negative_fresh("missing", 0));

    cache.remember_negative_at("missing".to_owned(), 10, 0);

    assert!(cache.negative_fresh("missing", 9));
    assert!(!cache.negative_fresh("missing", 10));
    assert!(!cache.negative_fresh("missing", 9));
}

#[test]
fn test_negative_cache_replacement_uses_the_new_deadline() {
    let cache = ServingCache::new(1024, 60);
    cache.remember_negative_at("missing".to_owned(), 10, 0);

    cache.remember_negative_at("missing".to_owned(), 20, 5);

    assert!(cache.negative_fresh("missing", 19));
    assert!(!cache.negative_fresh("missing", 20));
}

#[test]
fn test_negative_cache_default_clock_rejects_a_past_deadline() {
    let cache = ServingCache::new(1024, 60);

    cache.remember_negative("missing".to_owned(), 0);

    assert!(!cache.negative_fresh("missing", 0));
}

#[test]
fn test_negative_cache_default_clock_reclaims_an_expired_entry_and_keeps_a_live_one() {
    let cache = ServingCache::new(1024, 60);

    cache.remember_negative("expired".to_owned(), 2);
    cache.remember_negative("live".to_owned(), i64::MAX);

    cache.negative.run_pending_tasks();
    assert_eq!(cache.negative.entry_count(), 1);
    assert!(!cache.negative_fresh("expired", 1));
    assert!(cache.negative_fresh("live", 1));
}

#[test]
fn test_negative_cache_budget_is_eight_mebibytes() {
    let cache = ServingCache::new(1024, 60);

    assert_eq!(cache.negative.policy().max_capacity(), Some(8_388_608));
}

#[test]
fn test_negative_cache_accepts_an_entry_that_exactly_fills_its_byte_budget() {
    let cache = ServingCache::new(1024, 60);
    let capacity = usize::try_from(cache.negative.policy().max_capacity().unwrap()).unwrap();
    let mut key = String::with_capacity(capacity - usize::try_from(negative_weight(&String::new())).unwrap());
    key.push('x');
    assert_eq!(
        u64::from(negative_weight(&key)),
        cache.negative.policy().max_capacity().unwrap()
    );

    // The key moves in whole so its capacity, which the weigher counts, reaches the cache intact.
    cache.remember_negative_at(key, 10, 0);

    assert!(cache.negative_fresh("x", 9));
}

#[test]
fn test_negative_cache_maintenance_reclaims_expired_entries() {
    let cache = ServingCache::new(1024, 60);

    for key in ["first", "second"] {
        cache.remember_negative_at(key.to_owned(), 10, 0);
    }
    cache.remember_negative_at("fresh".to_owned(), 20, 10);

    assert_eq!(cache.negative.entry_count(), 1);
    assert!(cache.negative_fresh("fresh", 10));
}

#[test]
fn test_negative_cache_rejects_an_entry_over_its_byte_budget() {
    let cache = ServingCache::new(1024, 60);
    let mut key = String::with_capacity(usize::try_from(cache.negative.policy().max_capacity().unwrap()).unwrap());
    key.push('x');

    cache.remember_negative_at(key, 10, 0);

    cache.negative.run_pending_tasks();
    assert_eq!(cache.negative.entry_count(), 0);
}

#[test]
fn test_negative_cache_churn_stays_within_its_byte_budget() {
    let cache = ServingCache::new(1024, 60);
    let capacity = cache.negative.policy().max_capacity().unwrap();
    let name = "x".repeat(usize::try_from(capacity / 16).unwrap());

    for key in 0..64 {
        cache.remember_negative_at(format!("{key}-{name}"), i64::MAX, 0);
    }

    cache.negative.run_pending_tasks();
    assert!(cache.negative.weighted_size() <= capacity);
}

#[rstest]
#[case::unlimited(1_000_000, 0, 0, 60, true)]
#[case::inside_bound(1_359, 300, 1_000, 60, true)]
#[case::at_bound(1_360, 300, 1_000, 60, false)]
#[case::future_fetch(1_000, 300, 5_000, 60, true)]
#[case::saturating_window(1_000, 1, 0, i64::MAX, true)]
fn test_stale_bound(
    #[case] now: i64,
    #[case] max_stale_secs: i64,
    #[case] fetched_at: i64,
    #[case] freshness_secs: i64,
    #[case] expected: bool,
) {
    assert_eq!(
        within_stale_bound(now, max_stale_secs, fetched_at, freshness_secs),
        expected
    );
}
