use std::future::Future;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use rstest::rstest;

use super::{FlightGate, Inflight, ServingCache, flight_gate, release_flight, within_stale_bound};

#[derive(Default)]
struct FlightObserver {
    inflight: Inflight,
}

impl FlightObserver {
    fn gate(&self, key: &str) -> FlightGate {
        flight_gate(&self.inflight, key)
    }

    fn users(&self, key: &str) -> usize {
        self.inflight
            .gates
            .get(key)
            .map_or(0, |gate| gate.users.load(Ordering::Acquire))
    }
}

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
    let observer = FlightObserver::default();
    let producer = observer.gate("digest").lock_owned().await;
    {
        let mut waiting = std::pin::pin!(observer.gate("digest").lock_owned());
        assert!(matches!(
            waiting.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        assert_eq!(observer.users("digest"), 2);
    }
    assert_eq!(observer.users("digest"), 1);

    drop(producer);
    assert_eq!(observer.users("digest"), 0);
    drop(observer.gate("digest").try_lock_owned().unwrap());
}

#[tokio::test]
async fn test_activity_signals_registered_caller_changes() {
    let observer = FlightObserver::default();
    let first = observer.gate("digest");
    assert_eq!(observer.users("digest"), 1);

    let second = observer.gate("digest");
    assert_eq!(observer.users("digest"), 2);
    drop(second);
    assert_eq!(observer.users("digest"), 1);
    drop(first);
    assert_eq!(observer.users("digest"), 0);
}

#[tokio::test]
async fn test_release_flight_retires_the_gate() {
    let observer = FlightObserver::default();
    let flight = observer.gate("digest");

    release_flight(&observer.inflight, "digest", flight.try_lock_owned().unwrap());

    assert_eq!(observer.users("digest"), 0);
}

#[test]
fn test_forget_flight_retires_an_uncontended_gate() {
    let cache = ServingCache::new(1024, 60);
    let guard = flight_gate(&cache.inflight, "digest").try_lock_owned().unwrap();

    cache.forget_flight("digest");

    let replacement = flight_gate(&cache.inflight, "digest").try_lock_owned().unwrap();
    drop(guard);
    drop(replacement);
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
fn test_representation_keys_change_only_for_the_invalidated_resource() {
    let cache = ServingCache::new(1024, 60);
    let first = cache.representation_key("route", "first", "json");
    let second = cache.representation_key("route", "second", "json");

    cache.invalidate_resource("first");

    assert_ne!(cache.representation_key("route", "first", "json"), first);
    assert_eq!(cache.representation_key("route", "second", "json"), second);
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
