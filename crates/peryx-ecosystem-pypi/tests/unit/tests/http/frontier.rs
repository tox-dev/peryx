//! A replica holds a hosted project's derived views behind the readable frontier until they catch up.

use super::support::*;
use bytes::Bytes;
use peryx_driver::state::{REQUIRED_VIEWS, ReadableFrontier, SEARCH_VIEW, ViewBlock, readable_frontier};
use peryx_search::SearchParams;
use rstest::rstest;

const PERYXPKG_WHEEL: &str = "peryxpkg-1.0-py3-none-any.whl";

/// The number of search hits for `query` on `state`'s own derived index.
fn search_hits(state: &Arc<AppState>, query: &str) -> usize {
    state
        .search
        .search(
            &state.search_ctx(),
            SearchParams {
                query: query.to_owned(),
                ..SearchParams::default()
            },
        )
        .unwrap()
        .total
}

/// Every hosted read that carries a journal serial is held on a replica whose search view lags, across
/// the Simple HTML detail, PEP 691 JSON detail, legacy JSON, and Simple index representations.
#[rstest]
#[case::html("/hosted/simple/peryxpkg/", Some("text/html"))]
#[case::json("/hosted/simple/peryxpkg/", Some("application/json"))]
#[case::legacy("/hosted/peryxpkg/json", None)]
#[case::index("/hosted/simple/", Some("application/json"))]
#[tokio::test]
async fn test_replica_holds_hosted_reads_above_the_readable_frontier(#[case] uri: &str, #[case] accept: Option<&str>) {
    let h = harness().await;
    assert_eq!(
        upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await,
        StatusCode::OK
    );
    let replica = replica_state(&h);
    let (held, ..) = get(&replica, uri, accept).await;
    assert_eq!(held, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_replica_serves_a_hosted_page_once_the_search_view_catches_up() {
    let h = harness().await;
    assert_eq!(
        upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await,
        StatusCode::OK
    );
    let published = h.state.meta.current_serial().unwrap();
    assert!(published > 0, "the upload advanced the journal");
    let replica = replica_state(&h);

    let (held, ..) = get(&replica, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(held, StatusCode::NOT_FOUND);

    h.state.meta.set_view_frontier(SEARCH_VIEW, published).unwrap();
    let (served, _, body) = get(&replica, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(served, StatusCode::OK);
    assert!(body.contains("peryxpkg"));
}

#[tokio::test]
async fn test_primary_serves_a_hosted_page_regardless_of_the_search_view() {
    let h = harness().await;
    assert_eq!(
        upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await,
        StatusCode::OK
    );
    // The primary is never read-only, so a lagging search view never holds its reads.
    let (status, _, body) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("peryxpkg"));
}

/// A cached index reports its upstream serial, not the local journal the readable frontier governs, so a
/// replica serves it even though its own frontier sits at zero. The JSON representation streams straight
/// from the cache, while the HTML one resolves through the gate, so both accepts exercise the path that
/// decides a cached index is never held.
#[rstest]
#[case::json(Some("application/json"))]
#[case::html(Some("text/html"))]
#[tokio::test]
async fn test_replica_does_not_gate_a_cached_index(#[case] accept: Option<&str>) {
    let h = harness().await;
    let wheel = b"not a real archive";
    let digest = Digest::of(wheel);
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            detail_json(digest.as_str(), &file_url).into_bytes(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&h.server)
        .await;
    // The primary fetches and caches the proxy page.
    let (primed, ..) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(primed, StatusCode::OK);

    let replica = replica_state(&h);
    let (status, ..) = get(&replica, "/pypi/simple/flask/", accept).await;
    assert_eq!(status, StatusCode::OK);
}

/// A virtual index (`root/pypi`) that layers the hosted store surfaces the store's page, so a replica
/// holds the virtual read behind the readable frontier exactly as it holds the member's own hosted
/// route: the member's serial is not yet readable, so the virtual page is a not-found until the search
/// view catches up, then serves. Covers the Simple HTML and PEP 691 JSON representations.
#[rstest]
#[case::html("/root/pypi/simple/peryxpkg/", Some("text/html"))]
#[case::json("/root/pypi/simple/peryxpkg/", Some("application/json"))]
#[tokio::test]
async fn test_replica_holds_a_virtual_read_of_a_hosted_member_until_readable(
    #[case] uri: &str,
    #[case] accept: Option<&str>,
) {
    let h = harness().await;
    assert_eq!(
        upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await,
        StatusCode::OK
    );
    let published = h.state.meta.current_serial().unwrap();
    assert!(published > 0, "the upload advanced the journal");
    let replica = replica_state(&h);

    let (held, ..) = get(&replica, uri, accept).await;
    assert_eq!(
        held,
        StatusCode::NOT_FOUND,
        "the virtual index leaked a member below the frontier"
    );

    h.state.meta.set_view_frontier(SEARCH_VIEW, published).unwrap();
    let (served, _, body) = get(&replica, uri, accept).await;
    assert_eq!(served, StatusCode::OK);
    assert!(body.contains("peryxpkg"));
}

#[tokio::test]
async fn test_apply_replicated_changes_retires_only_the_changed_projects() {
    let h = harness().await;
    let hot_alpha = h.state.hot_key("hosted", "alpha", cache::SIMPLE_HTML);
    let hot_beta = h.state.hot_key("hosted", "beta", cache::SIMPLE_HTML);
    h.state
        .cache
        .store_hot(hot_alpha.clone(), Bytes::from_static(b"A"), 1_000_000);
    h.state
        .cache
        .store_hot(hot_beta.clone(), Bytes::from_static(b"B"), 1_000_000);
    assert!(h.state.hot_fresh(&hot_alpha).is_some());
    assert!(h.state.hot_fresh(&hot_beta).is_some());

    let driver = h.state.replicated_apply_drivers().next().unwrap().clone();
    let changed = vec![
        format!("pypi\u{0}p\u{0}hosted/alpha"),
        // The same project twice exercises the dedup, and a non-project key is ignored.
        format!("pypi\u{0}p\u{0}hosted/alpha"),
        "pypi\u{0}f\u{0}deadbeef".to_owned(),
    ];
    driver
        .apply_replicated_changes(h.state.serving.as_ref(), &changed)
        .unwrap();

    // alpha's epoch advanced, so its current key is a miss; beta's key is unchanged and still a hit.
    let hot_alpha_now = h.state.hot_key("hosted", "alpha", cache::SIMPLE_HTML);
    let hot_beta_now = h.state.hot_key("hosted", "beta", cache::SIMPLE_HTML);
    assert_ne!(hot_alpha_now, hot_alpha, "alpha's epoch advanced");
    assert_eq!(hot_beta_now, hot_beta, "beta's epoch is unchanged");
    assert!(
        h.state.hot_fresh(&hot_alpha_now).is_none(),
        "alpha's hot pages were retired"
    );
    assert!(h.state.hot_fresh(&hot_beta_now).is_some(), "beta's hot pages survived");
}

#[tokio::test]
async fn test_apply_replicated_changes_ignores_a_change_on_an_unknown_index() {
    let h = harness().await;
    let hot = h.state.hot_key("hosted", "alpha", cache::SIMPLE_HTML);
    h.state
        .cache
        .store_hot(hot.clone(), Bytes::from_static(b"A"), 1_000_000);
    assert!(h.state.hot_fresh(&hot).is_some());

    // A change keyed to an index this replica never configured resolves to no serving positions, so it
    // retires nothing rather than touching an unrelated project that shares the name.
    let driver = h.state.replicated_apply_drivers().next().unwrap().clone();
    driver
        .apply_replicated_changes(h.state.serving.as_ref(), &["pypi\u{0}p\u{0}ghost/alpha".to_owned()])
        .unwrap();

    assert_eq!(
        h.state.hot_key("hosted", "alpha", cache::SIMPLE_HTML),
        hot,
        "an unknown index advanced no epoch",
    );
    assert!(h.state.hot_fresh(&hot).is_some(), "alpha's hot pages survived");
}

#[test]
fn test_the_readable_frontier_is_the_minimum_regardless_of_apply_order() {
    // The gate reads the readable frontier as the lowest serial every required view reflects, a minimum
    // over a set, so out-of-order view updates converge on the same held serial.
    let ordered = std::collections::BTreeMap::from([(SEARCH_VIEW.to_owned(), 2u64), ("cache".to_owned(), 5)]);
    let reversed = std::collections::BTreeMap::from([("cache".to_owned(), 5u64), (SEARCH_VIEW.to_owned(), 2)]);
    let required = &[SEARCH_VIEW, "cache"];
    assert_eq!(
        readable_frontier(9, &ordered, required),
        readable_frontier(9, &reversed, required)
    );
    assert_eq!(
        readable_frontier(9, &ordered, required),
        ReadableFrontier {
            serial: 2,
            blocking: Some(SEARCH_VIEW.to_owned()),
        }
    );
}

#[test]
fn test_a_derived_view_frontier_survives_a_store_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    MetaStore::open(&path)
        .unwrap()
        .set_view_frontier(SEARCH_VIEW, 5)
        .unwrap();

    // Reopen from disk: the durable frontier survives a restart, so the gate recovers the same readable
    // serial an uninterrupted apply computes rather than exposing metadata a lost view never reflected.
    let reopened = MetaStore::open_existing(&path).unwrap();
    assert_eq!(reopened.view_frontier(SEARCH_VIEW).unwrap(), Some(5));
    let recovered = readable_frontier(5, &reopened.view_frontiers().unwrap(), REQUIRED_VIEWS);
    assert_eq!(
        recovered,
        ReadableFrontier {
            serial: 5,
            blocking: None
        }
    );
}

#[tokio::test]
async fn test_a_replicated_per_file_removal_retires_the_project_view() {
    let h = harness().await;
    assert_eq!(
        upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await,
        StatusCode::OK
    );
    let replica = replica_state(&h);
    // Build the replica's index from the shared store; peryxpkg is present on its hosted route and, once
    // merged, on the virtual index that layers it.
    assert!(
        search_hits(&replica, "peryxpkg") > 0,
        "the upload is indexed before the removal"
    );

    // A replicated removal deletes the only file's upload record. Its raw key names no project marker, so
    // before the fix a replica retired nothing; now it maps back to (hosted, peryxpkg).
    assert!(
        h.state
            .meta
            .delete_upload("hosted", "peryxpkg", PERYXPKG_WHEEL, 0)
            .unwrap(),
        "the upload record existed"
    );
    let driver = h.state.replicated_apply_drivers().next().unwrap().clone();
    driver
        .apply_replicated_changes(
            replica.serving.as_ref(),
            &[format!("pypi\u{0}u\u{0}hosted/peryxpkg/{PERYXPKG_WHEEL}")],
        )
        .unwrap();

    assert_eq!(
        search_hits(&replica, "peryxpkg"),
        0,
        "the project's document was retired on every index that served it",
    );
}

#[tokio::test]
async fn test_apply_reports_a_block_when_a_project_view_cannot_rebuild() {
    let h = harness().await;
    assert_eq!(
        upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await,
        StatusCode::OK
    );
    let replica = replica_state(&h);
    // A corrupt upload record makes deriving peryxpkg's document fail, so the rebuild cannot complete.
    h.state
        .meta
        .put_upload("hosted", "peryxpkg", PERYXPKG_WHEEL, b"not json")
        .unwrap();

    let driver = h.state.replicated_apply_drivers().next().unwrap().clone();
    let outcome = driver.apply_replicated_changes(
        replica.serving.as_ref(),
        &[format!("pypi\u{0}u\u{0}hosted/peryxpkg/{PERYXPKG_WHEEL}")],
    );

    assert_eq!(
        outcome,
        Err(ViewBlock {
            view: SEARCH_VIEW.to_owned(),
        }),
        "a failed required-view rebuild reports the blocking view so the caller holds the frontier",
    );
}
