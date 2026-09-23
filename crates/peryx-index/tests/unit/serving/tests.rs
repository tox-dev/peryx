use std::future::Future;
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use rstest::rstest;

use super::{
    Inflight, ResourceTickets, ServingCache, flight_gate, negative_weight, release_flight, resource_ticket_weight,
    within_stale_bound,
};

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
fn test_representation_keys_isolate_routes_and_resources() {
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
fn test_invalidation_makes_an_old_response_unreachable() {
    let cache = ServingCache::new(1024, 60);
    let old = cache.representation_key("route", "resource", "json");

    cache.invalidate_resource("route", "resource");
    let fresh = cache.representation_key("route", "resource", "json");
    cache.store_hot(old.clone(), Bytes::from_static(b"old"), 10);

    assert_ne!(fresh, old);
    assert_eq!(cache.hot_fresh(&fresh, 0), None);
}

#[test]
fn test_invalidation_handles_seen_and_unseen_resources() {
    let cache = ServingCache::new(1024, 60);
    let seen = cache.representation_key("route", "seen", "json");

    cache.invalidate_resource("route", "unseen");
    cache.invalidate_resource("route", "seen");

    assert_ne!(cache.representation_key("route", "seen", "json"), seen);
}

#[test]
fn test_invalidate_all_retires_every_ticket() {
    let cache = ServingCache::new(1024, 60);
    let old = [
        cache.representation_key("route", "first", "json"),
        cache.representation_key("independent", "second", "json"),
    ];

    cache.invalidate_all();

    let fresh = [
        cache.representation_key("route", "first", "json"),
        cache.representation_key("independent", "second", "json"),
    ];
    assert!(old.iter().zip(&fresh).all(|(old, fresh)| old != fresh));
}

#[test]
fn test_concurrent_invalidations_remove_one_ticket() {
    let cache = ServingCache::new(1024, 60);
    let old = cache.representation_key("route", "resource", "json");
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| cache.invalidate_resource("route", "resource"));
        }
    });

    assert_ne!(cache.representation_key("route", "resource", "json"), old);
}

#[test]
fn test_lookup_invalidation_and_refresh_do_not_reuse_a_ticket() {
    let cache = ServingCache::new(1024, 60);
    let barrier = std::sync::Barrier::new(2);
    let old = std::thread::scope(|scope| {
        let lookup = scope.spawn(|| {
            let old = cache.representation_key("route", "resource", "json");
            barrier.wait();
            old
        });
        barrier.wait();
        cache.invalidate_resource("route", "resource");
        lookup.join().unwrap()
    });

    assert_ne!(cache.representation_key("route", "resource", "json"), old);
}

#[test]
fn test_eviction_revisit_does_not_reuse_a_ticket() {
    let cache = ServingCache::new(1024, 60);
    let cohort = (0..64)
        .map(|index| format!("resource-{index}-{}", "x".repeat(131_072)))
        .collect::<Vec<_>>();
    let old = cohort
        .iter()
        .map(|resource| cache.representation_key("route", resource, "json"))
        .collect::<Vec<_>>();

    for resource in &cohort {
        let _ = cache.representation_key("route", resource, "json");
    }

    let fresh = cohort
        .iter()
        .map(|resource| cache.representation_key("route", resource, "json"))
        .collect::<Vec<_>>();
    let evicted = old
        .iter()
        .zip(&fresh)
        .filter(|(old, fresh)| old != fresh)
        .collect::<Vec<_>>();
    assert!(!evicted.is_empty());
    assert!(evicted.iter().all(|(old, _)| fresh.iter().all(|fresh| *old != fresh)));
}

#[test]
fn test_oversized_resource_names_get_fresh_unadmitted_tickets() {
    let cache = ServingCache::new(1024, 60);
    let resource = "x".repeat(8 * 1024 * 1024);

    assert_ne!(
        cache.representation_key("route", &resource, "json"),
        cache.representation_key("route", &resource, "json")
    );
}

#[rstest]
#[case::exactly_fills_the_budget(8_388_608, true)]
#[case::one_byte_over_the_budget(8_388_609, false)]
fn test_resource_ticket_admission_at_the_eight_mebibyte_budget(#[case] weight: usize, #[case] admitted: bool) {
    let mut tickets = ResourceTickets::new();

    let first = tickets.get_or_insert(ticket_name_weighing("x", weight));

    assert_eq!(tickets.get_or_insert("x".to_owned()) == first, admitted);
}

#[test]
fn test_resource_ticket_invalidation_releases_its_budget() {
    let mut tickets = ResourceTickets::new();
    let _ = tickets.get_or_insert(ticket_name_weighing("old", 8_388_608));

    tickets.invalidate("old");
    let fresh = tickets.get_or_insert(ticket_name_weighing("fresh", 8_388_608));

    assert_eq!(tickets.get_or_insert("fresh".to_owned()), fresh);
}

fn ticket_name_weighing(name: &str, weight: usize) -> String {
    let mut key = String::with_capacity(weight - usize::try_from(resource_ticket_weight(&String::new())).unwrap());
    key.push_str(name);
    key
}

#[test]
fn test_million_mutation_only_churn_does_not_allocate_tickets() {
    let cache = ServingCache::new(1024, 60);
    let before = cache.representation_key("route", "resource", "json");

    for resource in 0..1_000_000 {
        cache.invalidate_resource("route", &resource.to_string());
    }

    assert_eq!(cache.representation_key("route", "resource", "json"), before);
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
