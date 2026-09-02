use peryx_upstream::{Auth, CredentialIdentity, CredentialProvider};
use rstest::rstest;

use super::{DeclaredLifetime, MAX_BYTES, MAX_ENTRIES, TokenCache, TokenCacheKey, expires_at};

/// Each `fixed` provider carries its own identity, so two calls stand for two credential sources.
async fn identity() -> CredentialIdentity {
    CredentialProvider::fixed(Auth::None)
        .credential()
        .await
        .unwrap()
        .identity()
}

fn key(identity: CredentialIdentity, scope: &str) -> TokenCacheKey {
    TokenCacheKey {
        base: "https://registry.example/v2/".to_owned(),
        scope: scope.to_owned(),
        provider: identity.provider(),
    }
}

const LIVE_UNTIL: i64 = 1_000;

#[tokio::test]
async fn test_a_cached_token_comes_back_until_it_expires() {
    let identity = identity().await;
    let mut cache = TokenCache::default();
    cache.insert(
        key(identity, "repository:a:pull"),
        identity,
        "tok".to_owned(),
        LIVE_UNTIL,
        0,
    );

    let live = cache.get(&key(identity, "repository:a:pull"), identity, LIVE_UNTIL - 1);
    let expired = cache.get(&key(identity, "repository:a:pull"), identity, LIVE_UNTIL);

    assert_eq!((live, expired), (Some("tok".to_owned()), None));
}

#[tokio::test]
async fn test_a_token_issued_to_another_credential_generation_is_a_miss() {
    let issued = identity().await;
    let other = identity().await;
    let mut cache = TokenCache::default();
    cache.insert(
        key(issued, "repository:a:pull"),
        issued,
        "tok".to_owned(),
        LIVE_UNTIL,
        0,
    );

    assert_eq!(cache.get(&key(issued, "repository:a:pull"), other, 0), None);
}

#[tokio::test]
async fn test_distinct_scopes_stay_within_the_entry_budget() {
    let identity = identity().await;
    let mut cache = TokenCache::default();

    for scope in 0..MAX_ENTRIES + 200 {
        cache.insert(
            key(identity, &format!("repository:r{scope}:pull")),
            identity,
            "tok".to_owned(),
            LIVE_UNTIL,
            0,
        );
    }

    assert_eq!(cache.len(), MAX_ENTRIES);
    assert!(cache.bytes() <= MAX_BYTES);
}

#[tokio::test]
async fn test_outsized_tokens_stay_within_the_byte_budget() {
    let identity = identity().await;
    let mut cache = TokenCache::default();
    let token = "t".repeat(256 * 1024);

    for scope in 0..32 {
        cache.insert(
            key(identity, &format!("repository:r{scope}:pull")),
            identity,
            token.clone(),
            LIVE_UNTIL,
            0,
        );
    }

    assert!(cache.bytes() <= MAX_BYTES, "retained {} bytes", cache.bytes());
    assert!(cache.len() < 32);
}

/// One token past the whole budget leaves nothing behind, which is the arm where eviction runs the
/// order out rather than stopping part way.
#[tokio::test]
async fn test_a_token_larger_than_the_budget_is_not_retained() {
    let identity = identity().await;
    let mut cache = TokenCache::default();
    cache.insert(
        key(identity, "repository:a:pull"),
        identity,
        "tok".to_owned(),
        LIVE_UNTIL,
        0,
    );

    cache.insert(
        key(identity, "repository:b:pull"),
        identity,
        "t".repeat(MAX_BYTES + 1),
        LIVE_UNTIL,
        0,
    );

    assert_eq!((cache.len(), cache.bytes()), (0, 0));
}

/// The eviction order follows use, not insertion, so the repository a burst keeps pulling survives a
/// flood of scopes named once. Without that the burst would re-authenticate on every request.
#[tokio::test]
async fn test_a_scope_under_active_pull_outlives_a_flood_of_single_use_scopes() {
    let identity = identity().await;
    let hot = key(identity, "repository:hot:pull");
    let mut cache = TokenCache::default();
    cache.insert(hot.clone(), identity, "hot-token".to_owned(), LIVE_UNTIL, 0);

    for scope in 0..MAX_ENTRIES * 2 {
        cache.insert(
            key(identity, &format!("repository:r{scope}:pull")),
            identity,
            "tok".to_owned(),
            LIVE_UNTIL,
            0,
        );
        cache.get(&hot, identity, 0);
    }

    assert_eq!(cache.get(&hot, identity, 0), Some("hot-token".to_owned()));
}

/// An expired tuple leaves at the next exchange, with no lookup naming it, so a scope pulled once
/// stops costing anything.
#[tokio::test]
async fn test_expired_tuples_leave_without_a_lookup_for_them() {
    let identity = identity().await;
    let stale = key(identity, "repository:stale:pull");
    let mut cache = TokenCache::default();
    cache.insert(stale.clone(), identity, "stale-token".to_owned(), 10, 0);
    let before = (cache.len(), cache.bytes());

    cache.insert(
        key(identity, "repository:fresh:pull"),
        identity,
        "fresh".to_owned(),
        100,
        20,
    );

    assert_eq!(before, (1, stale.base.len() + stale.scope.len() + "stale-token".len()));
    assert_eq!(cache.len(), 1);
    assert_eq!(
        cache.bytes(),
        stale.base.len() + "repository:fresh:pull".len() + "fresh".len()
    );
}

#[tokio::test]
async fn test_replacing_a_tuple_counts_its_weight_once() {
    let identity = identity().await;
    let scope = key(identity, "repository:a:pull");
    let mut cache = TokenCache::default();
    cache.insert(scope.clone(), identity, "first".to_owned(), LIVE_UNTIL, 0);

    cache.insert(scope.clone(), identity, "second".to_owned(), LIVE_UNTIL, 0);

    assert_eq!(cache.len(), 1);
    assert_eq!(cache.bytes(), scope.base.len() + scope.scope.len() + "second".len());
}

/// The lifetime is read from the response's own fields; the token itself is never decoded.
#[rstest]
#[case::absent(DeclaredLifetime::Absent, None, Some(55))]
#[case::declared(DeclaredLifetime::Seconds(300), None, Some(295))]
#[case::capped(DeclaredLifetime::Seconds(86_400), None, Some(3_595))]
#[case::malformed(DeclaredLifetime::Malformed, None, None)]
#[case::zero(DeclaredLifetime::Seconds(0), None, None)]
#[case::negative(DeclaredLifetime::Seconds(-30), None, None)]
#[case::shorter_than_the_skew(DeclaredLifetime::Seconds(4), None, None)]
#[case::partly_spent(DeclaredLifetime::Seconds(300), Some(-60), Some(235))]
#[case::spent_entirely(DeclaredLifetime::Seconds(60), Some(-120), None)]
#[case::issued_in_the_future(DeclaredLifetime::Seconds(300), Some(60), Some(295))]
#[case::absent_with_issued_at(DeclaredLifetime::Absent, Some(-10), Some(45))]
fn expiry_follows_the_declared_lifetime(
    #[case] lifetime: DeclaredLifetime,
    #[case] issued_offset: Option<i64>,
    #[case] expected_remaining: Option<i64>,
) {
    let now = 10_000;

    let retained = expires_at(lifetime, issued_offset.map(|offset| now + offset), now);

    assert_eq!(retained, expected_remaining.map(|remaining| now + remaining));
}
